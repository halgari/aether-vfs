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
//! Lock order: registry → file → LRU index / retired diagnostics; neither of
//! those is ever held while a file lock is taken.
//!
//! # What a caller must do
//!
//! [`ReadCache::read`] answers `None` whenever it does not serve a read — too
//! large, not cacheable, the file changed, a fetch failed or came back short,
//! no room — and the caller then reads uncached exactly as it would have
//! without the cache. So a cache answer is always the bytes the director
//! would have returned, or no answer at all.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

/// Bytes per cached unit.
pub const DEFAULT_BLOCK: usize = 64 * 1024;
/// Most units one miss fetches (a sequential reader's run): 1 MiB.
pub const DEFAULT_MAX_RUN: usize = 16;
/// Reads shorter than this are served from the cache; others bypass it.
pub const DEFAULT_THRESHOLD: usize = 64 * 1024;
/// Units one file may hold at once: 16 MiB.
pub const DEFAULT_BLOCKS_PER_FILE: usize = 256;
/// Bytes the whole cache may hold, in-flight fetches included. The shim
/// takes it from `VFS_SHIM_READ_CACHE_MIB`.
pub const DEFAULT_MAX_BYTES: usize = 256 << 20;

/// A file goes cold after this many locality misses of evaluation…
const COLD_AFTER_MISSES: u32 = 16;
/// …if it averaged fewer hits than [`CacheConfig::cold_hits_per_miss`] per
/// miss. Only re-fetches count (see [`CacheConfig::cold_counts_first_fetch`])
/// and not misses the process-wide cap caused (see
/// `State::pressure_evicted`), so this fires only on a file read at random
/// across more units than it may hold, where each re-fetched 64 KiB unit
/// (~80 µs with the provider) costs more than the ~5 uncached small reads
/// (~14 µs each) it would have to save to pay for itself. Replaying a real
/// launch's reads, it never fires.
pub const DEFAULT_COLD_HITS_PER_MISS: u32 = 8;
/// Reads a cold file is served uncached before it is tried again, doubling
/// each time it goes cold again, up to [`COLD_MAX_READS`].
const COLD_READS: u32 = 4096;
const COLD_MAX_READS: u32 = 1 << 16;
/// A file whose block fetches fail (or come back short) this many times
/// goes cold too, rather than paying a failed block fetch before every
/// uncached read.
const COLD_AFTER_FAILED_FETCHES: u32 = 4;
/// Units of one file remembered as evicted by the process-wide cap.
const PRESSURE_MEMORY: usize = 4096;
/// Files whose diagnostics are kept after nothing holds them any more.
const RETIRED_DIAGS: usize = 4096;

/// How the cache is cut up. [`Default`] is what the shim uses.
#[derive(Debug, Clone, Copy)]
pub struct CacheConfig {
    /// Bytes per unit: what is stored, evicted and (at least) fetched.
    pub block: usize,
    /// Most units one fetch brings in, when a file is being read
    /// sequentially. `1` fetches exactly the missing unit every time.
    pub max_run: usize,
    pub threshold: usize,
    /// Most units one file holds at once.
    pub blocks_per_file: usize,
    pub max_bytes: usize,
    /// A file whose locality misses average fewer hits than this goes cold
    /// (see [`COLD_AFTER_MISSES`]); `0` never sends a file cold for that.
    pub cold_hits_per_miss: u32,
    /// Whether a file's first fetch of a unit counts against it. Off by
    /// default: a file being read into the cache for the first time misses
    /// on every unit it touches however good its locality, and judging it
    /// on those misses sent a 4 KiB reader of an 8 MiB region cold before
    /// it had warmed. Only a unit fetched **again** — after the file's own
    /// LRU dropped it — then counts.
    pub cold_counts_first_fetch: bool,
    /// How long a fetch is waited for, counted from when it started — the
    /// fetch's own deadline. A reader that finds another thread's fetch of
    /// its block waits at most what is left of this; a fetch older than it
    /// (its thread was killed, or its provider stopped answering) is given
    /// up on: the reader removes it and fetches the block itself.
    pub wait: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        CacheConfig {
            block: DEFAULT_BLOCK,
            max_run: DEFAULT_MAX_RUN,
            threshold: DEFAULT_THRESHOLD,
            blocks_per_file: DEFAULT_BLOCKS_PER_FILE,
            max_bytes: DEFAULT_MAX_BYTES,
            cold_hits_per_miss: DEFAULT_COLD_HITS_PER_MISS,
            cold_counts_first_fetch: false,
            wait: crate::RESPONSE_DEADLINE,
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
    fn add(&mut self, o: &FileDiag) {
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
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    pressure_misses: AtomicU64,
    declined: AtomicU64,
    fetches: AtomicU64,
    bytes_fetched: AtomicU64,
    evictions: AtomicU64,
    invalidations: AtomicU64,
    blocks_invalidated: AtomicU64,
    cold: AtomicU64,
    fetch_failures: AtomicU64,
    fetches_abandoned: AtomicU64,
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
    #[cfg(test)]
    pub(crate) fn cacheable(&self) -> bool {
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
    /// Since the file was last judged: reads that hit, and locality misses.
    hits: u32,
    misses: u32,
    /// Reads left to serve uncached while cold, and the next cold spell.
    cold_left: u32,
    cold_next: u32,
    /// Block fetches of this file that failed since it last went cold.
    failed_fetches: u32,
    /// Where the last fetch ended, and how many units the next one takes if
    /// it starts exactly there.
    next_seq: Option<u64>,
    run: usize,
    /// Units the process-wide cap evicted from this file: a miss on one is
    /// a capacity miss, not evidence of poor locality.
    pressure_evicted: HashSet<u64>,
    /// Units this file has fetched before (a bit each): a miss on one of
    /// them is a re-fetch, which says something about locality; a first
    /// fetch does not.
    fetched_before: Vec<u64>,
    diag: FileDiag,
}

impl State {
    /// Mark `idx` fetched; whether it had been before.
    fn refetch(&mut self, idx: u64) -> bool {
        let (w, b) = ((idx / 64) as usize, idx % 64);
        if self.fetched_before.len() <= w {
            self.fetched_before.resize(w + 1, 0);
        }
        let was = self.fetched_before[w] & (1 << b) != 0;
        self.fetched_before[w] |= 1 << b;
        was
    }
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

/// What a finished fetch gives its waiters: the first unit's index and the
/// run's bytes, or `None` if it failed.
type Fetched = Option<(u64, Arc<[u8]>)>;

/// One fetch of a run of units, which other readers of any of them can
/// wait for.
struct Flight {
    done: Mutex<Option<Fetched>>,
    cv: Condvar,
    started: Instant,
}

impl Flight {
    fn new() -> Self {
        Flight {
            done: Mutex::new(None),
            cv: Condvar::new(),
            started: Instant::now(),
        }
    }

    /// Whether the fetch has outlived `deadline` without finishing.
    fn overdue(&self, deadline: Duration) -> bool {
        self.started.elapsed() >= deadline
    }

    /// The first completion wins; later ones are ignored.
    fn complete(&self, v: Fetched) {
        let mut g = lock(&self.done);
        if g.is_none() {
            *g = Some(v);
            self.cv.notify_all();
        }
    }

    /// Wait until the fetch finishes or `deadline` after it started.
    /// `Err(())` if it had not finished by then.
    fn wait(&self, deadline: Duration) -> Result<Fetched, ()> {
        let left = deadline.saturating_sub(self.started.elapsed());
        let g = lock(&self.done);
        let (g, _) = self
            .cv
            .wait_timeout_while(g, left, |d| d.is_none())
            .unwrap_or_else(|e| e.into_inner());
        g.clone().ok_or(())
    }
}

struct Registry {
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
    files: Mutex<Registry>,
    /// Ready units by the tick they were filed under, oldest first: the
    /// process-wide LRU. A unit read since it was filed carries a newer
    /// tick of its own and is re-filed when it reaches the front, rather
    /// than on every hit, so a hit takes only its file's lock.
    lru: Mutex<BTreeMap<u64, (Weak<Entry>, u64)>>,
    /// Bytes held: ready units plus fetches in flight.
    used: AtomicUsize,
    tick: AtomicU64,
    counters: Counters,
    /// Diagnostics of files swept out of the registry, so the per-file
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
            files: Mutex::new(Registry {
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
    fn sweep(&self, reg: &mut Registry) {
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
    /// first: live ones and ones already swept out. Holds the registry for
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
            max_run: 1,
            cold_hits_per_miss: DEFAULT_COLD_HITS_PER_MISS,
            cold_counts_first_fetch: true,
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
            max_run: 1,
            cold_hits_per_miss: DEFAULT_COLD_HITS_PER_MISS,
            cold_counts_first_fetch: true,
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
            max_run: 1,
            cold_hits_per_miss: DEFAULT_COLD_HITS_PER_MISS,
            cold_counts_first_fetch: true,
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

    /// A fetch that never returns (its thread was killed mid-fetch, which
    /// runs no cleanup) costs at most one reader its deadline: the reader
    /// that times out removes it, and the next reader fetches afresh.
    #[test]
    fn a_fetch_that_never_returns_is_waited_for_once_and_then_replaced() {
        let c = ReadCache::new(CacheConfig {
            wait: Duration::from_millis(200),
            ..tiny().cfg
        });
        let f = reg(&c, "a", 100);
        let data = content(100);
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        std::thread::scope(|scope| {
            // The "dead" fetcher: blocks until the end of the test.
            let stuck = scope.spawn(|| {
                let mut buf = [0u8; 4];
                c.read(&f, 0, &mut buf, |_, _| {
                    let _ = lock(&release_rx).recv();
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
            // The second reader waits — but only out the fetch's deadline.
            let t = Instant::now();
            let mut buf = [0u8; 4];
            let second = c.read(&f, 0, &mut buf, |_, _| panic!("must wait, not fetch"));
            let waited = t.elapsed();
            assert_eq!(second, None);
            assert!(waited < Duration::from_secs(2), "waited {waited:?}");
            // The third must not wait at all: the dead slot is gone.
            let t = Instant::now();
            let third = c.read(&f, 0, &mut buf, |o, b| {
                let o = o as usize;
                b.copy_from_slice(&data[o..o + b.len()]);
                Ok(b.len())
            });
            let took = t.elapsed();
            assert_eq!(third, Some(4));
            assert_eq!(&buf, &data[0..4]);
            assert!(
                took < Duration::from_millis(100),
                "the third read waited {took:?}"
            );
            release_tx.send(()).unwrap();
            assert_eq!(stuck.join().unwrap(), None, "the late fetch is discarded");
        });
        assert_eq!(c.stats().fetches_abandoned, 1);
        let s = Source::new(content(100));
        assert_eq!(read(&c, &f, &s, 1, 3).unwrap(), &s.data[1..4]);
        assert_eq!(
            s.calls(),
            0,
            "the block the third reader fetched is the one held"
        );
    }

    /// A reader that finds a fetch already older than its deadline does not
    /// wait on it at all: it takes the slot over.
    #[test]
    fn a_fetch_older_than_its_deadline_is_taken_over_without_waiting() {
        let c = ReadCache::new(CacheConfig {
            wait: Duration::from_millis(50),
            ..tiny().cfg
        });
        let f = reg(&c, "a", 100);
        // A loading slot whose thread is gone: nothing will ever complete it.
        lock(&f.entry.state).slots.push(Slot {
            idx: 0,
            kind: SlotKind::Loading(Arc::new(Flight::new())),
        });
        std::thread::sleep(Duration::from_millis(60));
        let s = Source::new(content(100));
        let t = Instant::now();
        assert_eq!(read(&c, &f, &s, 0, 4).unwrap(), &s.data[0..4]);
        assert!(t.elapsed() < Duration::from_millis(40));
        assert_eq!(s.calls(), 1);
    }

    /// Fetches that keep failing are counted, and after a few the file goes
    /// cold instead of paying a failed block fetch before every read.
    #[test]
    fn a_file_whose_fetches_keep_failing_goes_cold() {
        let c = tiny();
        let f = reg(&c, "a", 100);
        let calls = AtomicUsize::new(0);
        let mut buf = [0u8; 4];
        for _ in 0..100 {
            let got = c.read(&f, 0, &mut buf, |_, b| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(b.len() - 1)
            });
            assert_eq!(got, None);
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            COLD_AFTER_FAILED_FETCHES as usize
        );
        let st = c.stats();
        assert_eq!(st.fetch_failures, COLD_AFTER_FAILED_FETCHES as u64);
        assert_eq!(st.cold, 1);
        assert_eq!(st.resident_bytes, 0);
    }

    // ---- runs, units, pressure, diagnostics ----------------------------------

    /// A 16-byte-unit cache whose runs grow to 4 units.
    fn runs() -> ReadCache {
        ReadCache::new(CacheConfig {
            block: 16,
            max_run: 4,
            threshold: 8,
            blocks_per_file: 64,
            max_bytes: 1 << 20,
            cold_hits_per_miss: DEFAULT_COLD_HITS_PER_MISS,
            cold_counts_first_fetch: true,
            wait: Duration::from_secs(10),
        })
    }

    #[test]
    fn sequential_misses_fetch_growing_runs_and_random_ones_a_single_unit() {
        let c = runs();
        let s = Source::new(content(16 * 64));
        let f = reg(&c, "seq", 16 * 64);
        // A sequential reader: 1-byte reads straight through.
        for off in 0..16 * 32u64 {
            assert_eq!(
                read(&c, &f, &s, off, 1).unwrap(),
                &s.data[off as usize..off as usize + 1]
            );
        }
        // Fetches of 1, 2, 4, 4, 4… units: offsets 0, 16, 48, 112, 176, …
        let offs = lock(&s.offsets).clone();
        assert_eq!(&offs[..5], &[0, 16, 48, 112, 176]);
        assert_eq!(
            c.stats().fetches,
            2 + 8,
            "32 units in runs of 1, 2, then 4s"
        );
        let before = c.stats().bytes_fetched;
        // A random reader starts every run over at one unit.
        let r = reg(&c, "rand", 16 * 64);
        let s2 = Source::new(content(16 * 64));
        for idx in [40u64, 3, 60, 17] {
            read(&c, &r, &s2, idx * 16 + 5, 1).unwrap();
        }
        assert_eq!(c.stats().bytes_fetched - before, 4 * 16, "one unit each");
    }

    #[test]
    fn a_run_stops_at_end_of_file_and_at_a_unit_already_held() {
        let c = runs();
        let s = Source::new(content(16 * 5 + 3));
        let f = reg(&c, "f", 16 * 5 + 3);
        read(&c, &f, &s, 48, 1).unwrap(); // unit 3 alone
        read(&c, &f, &s, 0, 1).unwrap(); // unit 0
        read(&c, &f, &s, 16, 1).unwrap(); // sequential: units 1, 2 — not 3, held
        assert_eq!(*lock(&s.offsets), vec![48, 0, 16]);
        assert_eq!(c.stats().bytes_fetched, 16 + 16 + 32);
        read(&c, &f, &s, 64, 1).unwrap(); // unit 4: not where the last run ended
        assert_eq!(c.stats().bytes_fetched, 16 + 16 + 32 + 16);
        read(&c, &f, &s, 81, 1).unwrap(); // sequential, but only 3 bytes are left
        assert_eq!(
            c.stats().bytes_fetched,
            16 + 16 + 32 + 16 + 3,
            "never past EOF"
        );
        for off in 0..16 * 5 + 3u64 {
            assert_eq!(
                read(&c, &f, &s, off, 1).unwrap(),
                &s.data[off as usize..off as usize + 1]
            );
        }
        assert_eq!(s.calls(), 5);
    }

    /// A small file costs its size, never a whole unit, with the defaults.
    #[test]
    fn a_unit_is_never_larger_than_its_file() {
        let c = ReadCache::default();
        let s = Source::new(content(4096));
        let f = reg(&c, "a.json", 4096);
        for off in (0..4096u64).step_by(100) {
            assert!(read(&c, &f, &s, off, 1).is_some());
        }
        let st = c.stats();
        assert_eq!(
            (st.fetches, st.bytes_fetched, st.resident_bytes),
            (1, 4096, 4096)
        );
    }

    #[test]
    fn a_waiter_on_a_run_gets_its_own_unit() {
        let c = runs();
        let f = reg(&c, "f", 16 * 8);
        let data = content(16 * 8);
        let mut buf = [0u8; 1];
        // Make the next miss a 2-unit run starting at unit 1.
        c.read(&f, 0, &mut buf, |o, b| {
            b.copy_from_slice(&data[o as usize..o as usize + b.len()]);
            Ok(b.len())
        })
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let rx = Mutex::new(rx);
        std::thread::scope(|scope| {
            let (c, f, data, rx) = (&c, &f, &data, &rx);
            let loader = scope.spawn(move || {
                let mut b1 = [0u8; 1];
                c.read(f, 16, &mut b1, |o, b| {
                    let _ = lock(rx).recv();
                    b.copy_from_slice(&data[o as usize..o as usize + b.len()]);
                    Ok(b.len())
                })
                .map(|_| b1[0])
            });
            while lock(&f.entry.state)
                .slots
                .iter()
                .filter(|s| matches!(s.kind, SlotKind::Loading(_)))
                .count()
                < 2
            {
                std::thread::yield_now();
            }
            let waiter = scope.spawn(move || {
                let mut b2 = [0u8; 3];
                c.read(f, 37, &mut b2, |_, _| panic!("unit 2 is in the run"))
                    .map(|_| b2)
            });
            std::thread::sleep(Duration::from_millis(20));
            tx.send(()).unwrap();
            assert_eq!(loader.join().unwrap(), Some(data[16]));
            assert_eq!(waiter.join().unwrap().unwrap(), data[37..40]);
        });
    }

    /// A miss on a unit the process-wide cap evicted is a capacity miss: it
    /// is counted as such and never sends the file cold.
    #[test]
    fn misses_caused_by_the_global_cap_do_not_count_against_locality() {
        let c = ReadCache::new(CacheConfig {
            block: 16,
            max_run: 1,
            threshold: 8,
            blocks_per_file: 64,
            max_bytes: 32,
            cold_hits_per_miss: 8,
            cold_counts_first_fetch: true,
            wait: Duration::from_secs(1),
        });
        let s = Source::new(content(16 * 40));
        let a = reg(&c, "a", 16 * 40);
        // `a` reads unit 0 and other files push it out, over and over: every
        // miss of `a` after the first is the cap's doing.
        for i in 0..100u64 {
            read(&c, &a, &s, i % 16, 1).unwrap();
            for k in 0..2 {
                let other = reg(&c, &format!("b{i}-{k}"), 16 * 40);
                read(&c, &other, &s, 0, 1).unwrap();
            }
        }
        let top = c.top_files(10);
        let da = top.iter().find(|r| r.path == "a").unwrap();
        assert!(da.diag.pressure_misses >= 90, "{da:?}");
        assert_eq!(
            da.diag.cold_guard, 0,
            "`a` reads one unit: not poor locality"
        );
        assert!(!da.cold_now);
        assert!(c.stats().pressure_misses >= 90);
    }

    #[test]
    fn the_per_file_table_names_the_busiest_files_and_why_they_went_cold() {
        let c = tiny(); // 2 units a file, guard at 8 hits a miss
        let s = Source::new(content(16 * 64));
        let busy = reg(&c, "data/skyrim.esm", 16 * 64);
        for off in 0..200u64 {
            read(&c, &busy, &s, off % 16, 1).unwrap();
        }
        let rand = reg(&c, "data/random.bsa", 16 * 64);
        for i in 0..20u64 {
            let _ = read(&c, &rand, &s, (i * 7 % 64) * 16, 1);
        }
        let bad = reg(&c, "data/broken.dds", 100);
        let mut buf = [0u8; 2];
        for _ in 0..COLD_AFTER_FAILED_FETCHES {
            let _ = c.read(&bad, 0, &mut buf, |_, _| Err(-5));
        }
        let top = c.top_files(2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].path, "data/skyrim.esm");
        assert_eq!(
            (
                top[0].diag.reads,
                top[0].diag.hits,
                top[0].diag.misses,
                top[0].diag.fetches
            ),
            (200, 199, 1, 1)
        );
        assert_eq!(top[0].diag.bytes_fetched, 16);
        assert_eq!(top[1].path, "data/random.bsa");
        assert_eq!(top[1].diag.cold_guard, 1);
        assert!(top[1].cold_now);
        let all = c.top_files(10);
        let b = all.iter().find(|r| r.path == "data/broken.dds").unwrap();
        assert_eq!((b.diag.cold_failures, b.diag.cold_guard), (1, 0));
    }

    #[test]
    fn a_swept_files_diagnostics_stay_in_the_table() {
        let c = tiny();
        let s = Source::new(content(100));
        {
            let f = reg(&c, "gone.txt", 3);
            read(&c, &f, &s, 0, 2).unwrap();
            c.invalidate_path(0, "unrelated"); // nothing to do with it
        }
        // Evict its unit, then sweep it out with many other registrations.
        for i in 0..8 {
            let f = reg(&c, &format!("big{i}"), 100);
            read(&c, &f, &s, 0, 1).unwrap();
        }
        for i in 0..SWEEP_MIN + 10 {
            let _ = reg(&c, &format!("x{i}"), 100);
        }
        assert!(!lock(&c.files).by_name.contains_key(&Name {
            root: 0,
            path: "gone.txt".into()
        }));
        let top = c.top_files(100);
        let g = top
            .iter()
            .find(|r| r.path == "gone.txt")
            .expect("retired diag kept");
        assert_eq!(
            (g.diag.reads, g.diag.fetches, g.diag.bytes_fetched),
            (1, 1, 3)
        );
    }

    /// With the defaults, random 4 KiB reads inside a region the file may
    /// hold warm up and stay cached — first fetches are not held against
    /// it — while the same reads over a file far larger than it may hold
    /// keep re-fetching what its own LRU dropped, and go cold.
    #[test]
    fn a_warming_region_stays_cached_and_a_file_read_at_random_goes_cold() {
        let mib = 1u64 << 20;
        let c = ReadCache::default();
        let region = reg(&c, "region.esm", (64 * mib) as usize);
        let wide = reg(&c, "wide.bsa", (64 * mib) as usize);
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut buf = vec![0u8; 4096];
        let mut fetch = |_: u64, b: &mut [u8]| Ok(b.len());
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let off = 16 * mib + x % (8 * mib - 4096);
            assert!(c.read(&region, off, &mut buf, &mut fetch).is_some());
        }
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let _ = c.read(&wide, x % (64 * mib - 4096), &mut buf, &mut fetch);
        }
        let top = c.top_files(2);
        let r = top.iter().find(|r| r.path == "region.esm").unwrap();
        assert_eq!(r.diag.cold_guard, 0, "{r:?}");
        assert!(r.diag.hits > 19_000, "{r:?}");
        let w = top.iter().find(|r| r.path == "wide.bsa").unwrap();
        assert!(w.diag.cold_guard >= 1, "{w:?}");
    }

    #[test]
    fn the_defaults_are_64k_units_runs_to_1mib_and_256mib() {
        let d = CacheConfig::default();
        assert_eq!((d.block, d.max_run, d.threshold), (64 << 10, 16, 64 << 10));
        assert_eq!((d.blocks_per_file, d.max_bytes), (256, 256 << 20));
        assert_eq!(ReadCache::default().stats().max_bytes, 256 << 20);
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
            max_run: 1,
            cold_hits_per_miss: DEFAULT_COLD_HITS_PER_MISS,
            cold_counts_first_fetch: true,
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
