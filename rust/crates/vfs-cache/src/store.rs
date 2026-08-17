//! Process-wide block store (sharded RAM tier + optional disk files).
//!
//! ## The three costs this design exists to avoid
//!
//! An earlier version of this file was correct and unusably slow: a spike
//! measured a 4 KiB read through it at **24 MiB/s against ~1400 MiB/s raw**,
//! making the cache roughly 70x more expensive than the FFI boundary it exists
//! to protect. The whole correctness suite passed throughout. The three causes,
//! and what replaced each:
//!
//! 1. **A hit cloned the whole block.** `get` returned `Vec<u8>`, so serving
//!    4 KiB out of a 1 MiB block allocated and memcpy'd 1 MiB, then discarded
//!    99.6 % of it. Blocks are now [`Block`] = `Arc<[u8]>` and a hit is a
//!    refcount bump; the caller copies only the range it asked for, and does it
//!    **after the lock is released**.
//! 2. **A hit scanned the LRU ordering.** `order.iter().position(..)` on a
//!    `VecDeque` is O(n) in resident blocks. Replaced by **CLOCK** (second
//!    chance): a hit sets one reference bit and never touches the ordering, so
//!    it is O(1) exactly — not amortised. Eviction sweeps a hand over the ring
//!    and is amortised O(1), since a block can only earn a second chance by
//!    having been hit.
//! 3. **One process-wide `Mutex` serialised every reader.** Two changes, in
//!    order of importance: the reference bit is an `AtomicBool` inside the
//!    entry, so **a hit needs only a shared lock** and readers of the same
//!    shard no longer exclude each other at all; and the table is **sharded**
//!    so misses (which do need exclusive access) contend only with misses on
//!    the same shard.
//!
//! A fourth cost was not in the original report and only became visible once the
//! lock stopped hiding it: **the cache's own hit counters.** Two process-wide
//! `AtomicU64` increments per hit held 4-thread scaling to 1.35x; they are now
//! per-shard. See `Shard::hits`.
//!
//! ## Two tiers, one invalidation
//!
//! There are two places a block can be served from — a shard's RAM map and a
//! `.blk` file on disk — and they do not have the same lifetime: eviction moves a
//! block from the first to only the second, and the second survives the process.
//! Anything that has to make a block *stop* being servable therefore has to name
//! both, from each tier's own source of truth. See [`BlockCache::invalidate_file`]
//! for the corruption that followed from enumerating only the RAM map.
//!
//! CLOCK is an LRU approximation, so eviction order is no longer exactly LRU.
//! That is the deliberate trade: it is what makes a hit lock-shared and
//! ordering-free, and for the workload this cache exists for — an immutable
//! high-latency source, read repeatedly — the distinction between LRU and
//! second-chance is a detail, while "is a hit O(1) and parallel" is not.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

/// A cached block payload. Cloning is a refcount bump, never a copy of the
/// bytes — that property is the whole point, so the alias is public and the
/// hit path's return type names it.
pub type Block = Arc<[u8]>;

/// Cache key: source instance + stable file identity + block index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub source_id: u64,
    pub file_id: u64,
    pub block_index: u64,
}

#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub block_size: u64,
    /// Max RAM bytes for block payloads (not counting index overhead).
    pub ram_budget: u64,
    /// Optional directory for on-disk block files.
    pub disk_dir: Option<PathBuf>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            block_size: crate::DEFAULT_BLOCK_SIZE,
            ram_budget: 64 * 1024 * 1024,
            disk_dir: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub ram_evicts: u64,
    pub disk_hits: u64,
    pub disk_writes: u64,
    pub bytes_from_cache: u64,
    pub bytes_from_source: u64,
    pub ram_bytes: u64,
    pub ram_blocks: u64,
    /// Blocks handed to [`BlockCache::put`] that the RAM tier refused because a
    /// single one of them would not fit in a shard. **Any non-zero value here
    /// means the cache is not caching**: every read of such a block misses and
    /// goes back to the source. It is counted because the alternative — the
    /// original behaviour — was to drop the block and say nothing, which is
    /// indistinguishable from working while being 100 % slower than no cache at
    /// all. See [`BlockCache::max_cacheable_block`].
    pub oversized_rejects: u64,
    /// Tiers or entries [`BlockCache::invalidate_file`] could not clear, summed
    /// over every call. Non-zero means a mutated file may still have stale
    /// blocks somewhere in the cache.
    pub invalidate_failures: u64,
}

/// What one [`BlockCache::invalidate_file`] call managed to do.
///
/// Returned rather than swallowed because the caller is normally a provider that
/// has *just mutated the file*. If invalidation did not complete, the next read
/// can be served pre-write bytes, and the caller is the only party in a position
/// to decide what to do about that — see [`crate::CachingProvider`]'s
/// `write_at`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Invalidation {
    /// Blocks dropped from the RAM tier.
    pub ram_dropped: u64,
    /// `.blk` files removed from the disk tier.
    pub disk_dropped: u64,
    /// Things that could not be cleared: a shard whose lock was poisoned, a disk
    /// directory that could not be enumerated, or a `.blk` file that could not be
    /// removed. Non-zero means the cache may still be able to serve a stale block
    /// for this file.
    pub failures: u64,
}

impl Invalidation {
    /// True when every tier that could serve this file was cleared, and so the
    /// cache is guaranteed to have no stale block left for it.
    pub fn is_complete(&self) -> bool {
        self.failures == 0
    }
}

struct RamEntry {
    data: Block,
    /// CLOCK reference bit. Set by a hit through a *shared* borrow — this being
    /// an atomic rather than a `bool` is exactly what lets the hit path take a
    /// read lock instead of an exclusive one.
    referenced: AtomicBool,
}

/// One shard: a map, a CLOCK ring, its own byte budget, and its own hit
/// counters.
///
/// `map` and `ring` are kept in exact 1:1 correspondence — every key in the map
/// appears in the ring exactly once and vice versa — so the eviction sweep never
/// has to reason about stale ring entries. Every mutation below preserves that.
///
/// Aligned to a cache line so that two shards never share one: the point of a
/// shard is that work on it does not disturb work on another, and false sharing
/// would hand that back.
#[repr(align(64))]
struct Shard {
    map: HashMap<BlockKey, RamEntry>,
    ring: VecDeque<BlockKey>,
    bytes: u64,
    /// Hit counters live **per shard**, not on `BlockCache`.
    ///
    /// This is not premature: process-wide `AtomicU64`s here were measured as
    /// the largest remaining scalability limit after the lock was fixed. Two
    /// atomic read-modify-writes per hit, on two cache lines shared by every
    /// thread in the process, held 4-thread scaling to **1.35x**; moving them
    /// onto the shard raised it to **2.1x** with nothing else changed. A hit is
    /// now only ~70 ns, so a contended counter is not a rounding error on it —
    /// it is most of it. `misses` and the disk/evict counters stay global: each
    /// is followed by real I/O that dwarfs the increment.
    hits: AtomicU64,
    bytes_from_cache: AtomicU64,
}

/// Never shard so finely that a shard cannot hold a working set. Below this many
/// blocks per shard, sharding trades away capacity (hash imbalance evicts blocks
/// a single global budget would have kept) for concurrency it cannot deliver.
const MIN_BLOCKS_PER_SHARD: u64 = 8;
/// Upper bound on shards. Four per logical core on a 16-core machine, which is
/// the ratio at which lock-word collisions between independent readers become
/// rare. Deliberately a constant rather than a function of
/// `available_parallelism()`: shard geometry stays identical on every machine, so
/// it can be asserted on directly instead of inferred from timing, and the cost
/// of being generous is a few KiB of index against a multi-MiB budget.
const MAX_SHARDS: usize = 64;

/// Thread-safe block cache.
pub struct BlockCache {
    cfg: CacheConfig,
    shards: Box<[RwLock<Shard>]>,
    /// `shards.len() - 1`; `shards.len()` is always a power of two.
    shard_mask: usize,
    /// Per-shard byte budget. Sums to at most `cfg.ram_budget`, so
    /// `stats().ram_bytes <= ram_budget` still holds exactly.
    shard_budget: u64,
    misses: AtomicU64,
    ram_evicts: AtomicU64,
    disk_hits: AtomicU64,
    disk_writes: AtomicU64,
    bytes_from_source: AtomicU64,
    /// See [`CacheStats::oversized_rejects`].
    oversized_rejects: AtomicU64,
    /// See [`CacheStats::invalidate_failures`].
    invalidate_failures: AtomicU64,
    /// Whether the oversized-block warning has already been printed. The
    /// condition is a static misconfiguration, so without this it would repeat
    /// on every miss for the life of the process and become noise nobody reads.
    warned_oversized: AtomicBool,
}

impl BlockCache {
    pub fn new(cfg: CacheConfig) -> Self {
        if let Some(dir) = &cfg.disk_dir {
            let _ = std::fs::create_dir_all(dir);
        }
        // Shard count follows from the budget: enough shards to spread misses,
        // never so many that a shard holds fewer than MIN_BLOCKS_PER_SHARD.
        // A tiny budget (the small-fixture tests, `ram_budget: 0`) collapses to
        // one shard, which behaves exactly like the unsharded original.
        let capacity_shards = cfg.ram_budget / cfg.block_size.max(1) / MIN_BLOCKS_PER_SHARD;
        // Largest power of two that is <= both bounds. Rounding *down* matters:
        // rounding up would push a shard's budget below MIN_BLOCKS_PER_SHARD,
        // which is the thing `capacity_shards` was computed to prevent.
        let mut n = 1usize;
        while n * 2 <= MAX_SHARDS && (n as u64) * 2 <= capacity_shards {
            n *= 2;
        }
        let shards: Vec<RwLock<Shard>> = (0..n)
            .map(|_| {
                RwLock::new(Shard {
                    map: HashMap::new(),
                    ring: VecDeque::new(),
                    bytes: 0,
                    hits: AtomicU64::new(0),
                    bytes_from_cache: AtomicU64::new(0),
                })
            })
            .collect();
        Self {
            shard_mask: n - 1,
            shard_budget: cfg.ram_budget / n as u64,
            cfg,
            shards: shards.into_boxed_slice(),
            misses: AtomicU64::new(0),
            ram_evicts: AtomicU64::new(0),
            disk_hits: AtomicU64::new(0),
            disk_writes: AtomicU64::new(0),
            bytes_from_source: AtomicU64::new(0),
            oversized_rejects: AtomicU64::new(0),
            invalidate_failures: AtomicU64::new(0),
            warned_oversized: AtomicBool::new(false),
        }
    }

    pub fn block_size(&self) -> u64 {
        self.cfg.block_size.max(1)
    }

    /// The largest block the RAM tier will accept: a `put` of anything bigger is
    /// dropped, so the cache would hold nothing and every read of such a block
    /// would go back to the source.
    ///
    /// It is the **per-shard** budget, not `ram_budget`, and that is the whole
    /// reason this is public. Shard geometry is chosen in [`Self::new`] from
    /// `cfg.block_size`, while the block size actually stored is chosen per
    /// provider from `Capabilities::preferred_block` — the two are decoupled, so
    /// a caller that gets to pick its own block size has to be able to ask what
    /// will actually fit. [`crate::CachingProvider`] does exactly that.
    ///
    /// Zero means the RAM tier is off (`ram_budget: 0`), which is a legitimate
    /// disk-only configuration rather than a misconfiguration.
    pub fn max_cacheable_block(&self) -> u64 {
        self.shard_budget
    }

    /// Number of shards the RAM tier is split across. Exposed so a test can
    /// assert on shard geometry rather than infer it from timing.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    fn shard(&self, key: &BlockKey) -> &RwLock<Shard> {
        // splitmix64 finalizer over the three key fields. Consecutive
        // `block_index` values must land on different shards — a sequential
        // sweep is the common access pattern, and a low-bit-preserving hash
        // would put a whole run of blocks on one shard and undo the sharding.
        let mut h = key.source_id
            ^ key.file_id.rotate_left(21)
            ^ key.block_index.rotate_left(43);
        h ^= h >> 30;
        h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h ^= h >> 27;
        h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
        h ^= h >> 31;
        &self.shards[(h as usize) & self.shard_mask]
    }

    pub fn stats(&self) -> CacheStats {
        let mut ram_bytes = 0u64;
        let mut ram_blocks = 0u64;
        let mut hits = 0u64;
        let mut bytes_from_cache = 0u64;
        for s in self.shards.iter() {
            if let Ok(g) = s.read() {
                ram_bytes += g.bytes;
                ram_blocks += g.map.len() as u64;
                hits += g.hits.load(Ordering::Relaxed);
                bytes_from_cache += g.bytes_from_cache.load(Ordering::Relaxed);
            }
        }
        CacheStats {
            hits,
            misses: self.misses.load(Ordering::Relaxed),
            ram_evicts: self.ram_evicts.load(Ordering::Relaxed),
            disk_hits: self.disk_hits.load(Ordering::Relaxed),
            disk_writes: self.disk_writes.load(Ordering::Relaxed),
            bytes_from_cache,
            bytes_from_source: self.bytes_from_source.load(Ordering::Relaxed),
            ram_bytes,
            ram_blocks,
            oversized_rejects: self.oversized_rejects.load(Ordering::Relaxed),
            invalidate_failures: self.invalidate_failures.load(Ordering::Relaxed),
        }
    }

    /// Look up a block. On a RAM hit this takes a **shared** lock, bumps the
    /// CLOCK reference bit, and returns a refcounted handle to the payload —
    /// no allocation, no copy, and no touch of the eviction ordering. The
    /// caller copies out only the range it needs, with the lock already
    /// released.
    pub fn get(&self, key: &BlockKey) -> Option<Block> {
        let hit = match self.shard(key).read() {
            Ok(g) => g.map.get(key).map(|e| {
                e.referenced.store(true, Ordering::Relaxed);
                // Counted on the shard, inside the guard already held: no extra
                // lock, and no cache line shared with any other shard.
                g.hits.fetch_add(1, Ordering::Relaxed);
                g.bytes_from_cache
                    .fetch_add(e.data.len() as u64, Ordering::Relaxed);
                Block::clone(&e.data)
            }),
            Err(_) => None,
        };
        if let Some(data) = hit {
            return Some(data);
        }
        if let Some(path) = self.disk_path(key) {
            if let Ok(data) = std::fs::read(&path) {
                let data: Block = data.into();
                self.disk_hits.fetch_add(1, Ordering::Relaxed);
                // Cold path — a file read just happened — so re-taking the
                // shard's read lock purely to count is free by comparison, and
                // it keeps one code path as the only writer of these counters.
                if let Ok(g) = self.shard(key).read() {
                    g.hits.fetch_add(1, Ordering::Relaxed);
                    g.bytes_from_cache
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                }
                self.insert_ram(*key, Block::clone(&data));
                return Some(data);
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Insert a filled block (from source).
    ///
    /// Takes `impl Into<Block>`: a caller holding a `Vec<u8>` pays one copy into
    /// the `Arc` allocation (the same single copy the old `put(Vec)` cost), and a
    /// caller that already has a [`Block`] — the miss path in
    /// [`crate::CachingProvider`] — pays none.
    pub fn put(&self, key: BlockKey, data: impl Into<Block>) {
        let data: Block = data.into();
        self.bytes_from_source
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        if let Some(path) = self.disk_path(&key) {
            if std::fs::write(&path, &data[..]).is_ok() {
                self.disk_writes.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.insert_ram(key, data);
    }

    /// Drop every cached block for a file, in **every tier that can serve it**.
    ///
    /// ## Why the key set is not taken from the RAM map
    ///
    /// It cannot be, and taking it from there was a live data-corruption bug. The
    /// disk tier outlives RAM residency in both directions: a block evicted from
    /// RAM keeps its `.blk` file, and the directory outlives the process
    /// entirely. So the RAM map is a list of *some* of the blocks that can be
    /// served, never all of them, and the ones it omits are exactly the ones
    /// eviction has already moved out of sight. The observable result was that
    /// [`crate::CachingProvider`]'s `write_at` invalidated, reported success, and
    /// the next read of the written range handed back the pre-write bytes out of
    /// `.blk`.
    ///
    /// Each tier is therefore enumerated from its own source of truth: the shard
    /// maps for RAM, the directory listing for disk. The disk sweep matches the
    /// `{source_id:x}_{file_id:x}_` filename prefix instead of a computed range
    /// of block indices, which also clears `.blk` files left by an earlier run
    /// with a **different block size** — their block indices are ones this run
    /// would never generate, and they are stale for the same reason.
    ///
    /// The return value says whether that actually succeeded; see
    /// [`Invalidation`].
    #[must_use = "invalidation can fail partially, and then the cache can still \
                  serve pre-write bytes — a caller that has just mutated the \
                  file must decide what to do about that"]
    pub fn invalidate_file(&self, source_id: u64, file_id: u64) -> Invalidation {
        let mut out = Invalidation::default();
        // **Disk before RAM, deliberately.** A concurrent `get` that misses RAM
        // falls through to the disk tier and re-inserts what it finds there. With
        // RAM cleared first, such a `get` would read a `.blk` this call is about to
        // delete and put the stale block straight back into the map behind us.
        // Clearing disk first closes that: after this line a falling-through `get`
        // finds nothing and refetches from the source.
        //
        // It does not close the window completely — a `get` that had already read
        // the file bytes before the unlink can still insert them after the sweep
        // below — and nothing short of a per-file epoch would, which is a bigger
        // change than this defect needs. The narrow window is a read racing a
        // write on the same file with no ordering between them, where the reader
        // may legitimately observe either version; the bug this fixes was a read
        // strictly *after* a completed write seeing the old bytes.
        self.invalidate_on_disk(source_id, file_id, &mut out);
        // Cold path: a whole-file sweep of every shard. O(resident) by nature —
        // it has to find the file's blocks — and it is not the hit path.
        for s in self.shards.iter() {
            let Ok(mut g) = s.write() else {
                // A poisoned shard keeps whatever it holds, and what it holds is
                // now stale. Countable, not ignorable.
                out.failures += 1;
                continue;
            };
            let keys: Vec<BlockKey> = g
                .map
                .keys()
                .filter(|k| k.source_id == source_id && k.file_id == file_id)
                .copied()
                .collect();
            if keys.is_empty() {
                continue;
            }
            for k in &keys {
                if let Some(e) = g.map.remove(k) {
                    g.bytes = g.bytes.saturating_sub(e.data.len() as u64);
                    out.ram_dropped += 1;
                }
            }
            // One pass to restore the map/ring 1:1 invariant, not one per key.
            g.ring
                .retain(|k| !(k.source_id == source_id && k.file_id == file_id));
        }
        if !out.is_complete() {
            self.invalidate_failures
                .fetch_add(out.failures, Ordering::Relaxed);
        }
        out
    }

    /// The disk half of [`Self::invalidate_file`]: remove every `.blk` belonging
    /// to this file, RAM-resident or not, by listing the directory rather than by
    /// asking any in-memory structure what it thinks is there.
    fn invalidate_on_disk(&self, source_id: u64, file_id: u64, out: &mut Invalidation) {
        let Some(dir) = self.cfg.disk_dir.as_ref() else {
            return;
        };
        let prefix = Self::disk_prefix(source_id, file_id);
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            // No directory at all means the disk tier holds nothing for anyone.
            // That is the goal already met, not a failure to reach it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => {
                out.failures += 1;
                return;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                // An entry that cannot be read might be one of this file's
                // blocks, so it counts against completeness.
                out.failures += 1;
                continue;
            };
            let name = entry.file_name();
            // A name that is not valid UTF-8 cannot be one this cache wrote:
            // `disk_path` only ever emits hex, `_` and `.blk`.
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with(&prefix) || !name.ends_with(".blk") {
                continue;
            }
            match std::fs::remove_file(entry.path()) {
                Ok(()) => out.disk_dropped += 1,
                // Something else removed it between the listing and now; the
                // block is gone either way, which is all this call wanted.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => out.failures += 1,
            }
        }
    }

    /// CLOCK second-chance sweep: make room for `need` bytes in `g`.
    ///
    /// Terminates because every iteration strictly decreases one of three
    /// quantities: bytes resident (an eviction), ring length (a key already gone
    /// from the map), or `chances`. `chances` starts at the ring length, so a
    /// ring in which *every* entry is referenced clears bits for one full lap
    /// and evicts on the next candidate — never loops.
    fn evict_to_fit(&self, g: &mut Shard, need: u64) {
        let mut chances = g.ring.len();
        while g.bytes + need > self.shard_budget {
            let Some(cand) = g.ring.pop_front() else { break };
            let referenced = match g.map.get(&cand) {
                // Cannot happen while the 1:1 invariant holds; dropping the ring
                // entry is the self-healing response if it ever does not.
                None => continue,
                Some(e) => e.referenced.swap(false, Ordering::Relaxed),
            };
            if referenced && chances > 0 {
                chances -= 1;
                g.ring.push_back(cand);
                continue;
            }
            if let Some(e) = g.map.remove(&cand) {
                g.bytes = g.bytes.saturating_sub(e.data.len() as u64);
                self.ram_evicts.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn insert_ram(&self, key: BlockKey, data: Block) {
        let len = data.len() as u64;
        // A block that cannot fit in a shard on its own is not cacheable, and
        // dropping it here is silent by construction: `put` returns nothing, so
        // the caller sees an insert that looks like it worked and the next read
        // misses again. Forever. The comment that used to sit here claimed this
        // was unreachable with more than one shard. It was not:
        //
        // * shard geometry is derived from `cfg.block_size`, but
        // * the block size actually stored is chosen **per provider** from
        //   `Capabilities::preferred_block` (see `CachingProvider::new`).
        //
        // So a source hinting 4 MiB against a cache configured for 4 KiB blocks
        // with a 64 MiB budget lands 4 MiB blocks in shards with a 1 MiB budget
        // and every single `put` returns here. Measured on that configuration:
        // 20 MiB pulled from the leaf to satisfy five 4 KiB reads — strictly
        // worse than no cache, and nothing anywhere said so.
        //
        // `CachingProvider` now clamps its block size to `max_cacheable_block()`
        // so it cannot construct that state. This counts and reports whatever
        // still arrives here, because a cache that has quietly stopped caching is
        // the one failure mode no other signal in the system exposes.
        if len > self.shard_budget {
            self.oversized_rejects.fetch_add(1, Ordering::Relaxed);
            self.warn_oversized(len);
            return;
        }
        let Ok(mut g) = self.shard(&key).write() else {
            return;
        };
        if let Some(old) = g.map.remove(&key) {
            g.bytes = g.bytes.saturating_sub(old.data.len() as u64);
            // O(n) in this shard's ring, and deliberately so: this branch is
            // *replacing* an existing key, which happens only when two threads
            // miss the same block at once or a block is refilled after
            // invalidation. Keeping the ring free of duplicates is what lets the
            // sweep above trust `map.get`. The hit path never reaches here.
            g.ring.retain(|k| k != &key);
        }
        self.evict_to_fit(&mut g, len);
        g.bytes += len;
        g.map.insert(
            key,
            RamEntry {
                data,
                referenced: AtomicBool::new(false),
            },
        );
        g.ring.push_back(key);
    }

    /// Say once, on stderr, that the RAM tier is refusing everything it is being
    /// given. Once per cache rather than once per `put`: the condition is static,
    /// so per-put would be a flood nobody reads, and the counter in
    /// [`CacheStats::oversized_rejects`] is the machine-readable channel anyway.
    fn warn_oversized(&self, len: u64) {
        // A zero shard budget is `ram_budget: 0` — "no RAM tier", asked for
        // explicitly by the disk-only configuration. Refusing every block is
        // then the requested behaviour and not worth a word.
        if self.shard_budget == 0 {
            return;
        }
        if self.warned_oversized.swap(true, Ordering::Relaxed) {
            return;
        }
        eprintln!(
            "vfs-cache: NOT caching {len} byte blocks — one does not fit a shard's \
             {} byte budget ({} byte ram_budget split across {} shards). Every \
             read of a block this size will go to the source, so the cache is \
             costing a full block fetch per read and returning nothing. Raise \
             ram_budget or lower the block size.",
            self.shard_budget,
            self.cfg.ram_budget,
            self.shards.len()
        );
    }

    /// The `{source_id:x}_{file_id:x}_` prefix shared by every one of a file's
    /// `.blk` names. The **trailing underscore matters**: without it, the prefix
    /// for file id `0x2` would also match file id `0x20`'s files and invalidating
    /// one file would silently delete another's cached blocks.
    fn disk_prefix(source_id: u64, file_id: u64) -> String {
        format!("{source_id:x}_{file_id:x}_")
    }

    fn disk_path(&self, key: &BlockKey) -> Option<PathBuf> {
        let dir = self.cfg.disk_dir.as_ref()?;
        // Built from `disk_prefix` rather than repeating the format string, so
        // the writer of these names and the invalidation sweep that matches them
        // cannot drift apart.
        Some(dir.join(format!(
            "{}{:x}.blk",
            Self::disk_prefix(key.source_id, key.file_id),
            key.block_index
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_hit_and_miss() {
        let c = BlockCache::new(CacheConfig {
            block_size: 64,
            ram_budget: 1024,
            disk_dir: None,
        });
        let key = BlockKey {
            source_id: 1,
            file_id: 2,
            block_index: 0,
        };
        assert!(c.get(&key).is_none());
        c.put(key, vec![7u8; 32]);
        // `get` returns a `Block` (`Arc<[u8]>`) rather than a `Vec<u8>`, so the
        // comparison is by slice. Same bytes asserted, same meaning.
        assert_eq!(&c.get(&key).unwrap()[..], &[7u8; 32][..]);
        let s = c.stats();
        assert_eq!(s.misses, 1);
        assert_eq!(s.hits, 1);
    }

    #[test]
    fn eviction_under_budget() {
        let c = BlockCache::new(CacheConfig {
            block_size: 8,
            ram_budget: 16,
            disk_dir: None,
        });
        c.put(
            BlockKey {
                source_id: 0,
                file_id: 0,
                block_index: 0,
            },
            vec![1u8; 8],
        );
        c.put(
            BlockKey {
                source_id: 0,
                file_id: 0,
                block_index: 1,
            },
            vec![2u8; 8],
        );
        c.put(
            BlockKey {
                source_id: 0,
                file_id: 0,
                block_index: 2,
            },
            vec![3u8; 8],
        );
        assert!(c.stats().ram_evicts >= 1);
        assert!(c.stats().ram_bytes <= 16);
    }

    #[test]
    fn invalidate_file_drops_blocks() {
        let c = BlockCache::new(CacheConfig {
            block_size: 8,
            ram_budget: 1024,
            disk_dir: None,
        });
        let k0 = BlockKey {
            source_id: 1,
            file_id: 9,
            block_index: 0,
        };
        let k1 = BlockKey {
            source_id: 1,
            file_id: 9,
            block_index: 1,
        };
        let other = BlockKey {
            source_id: 1,
            file_id: 10,
            block_index: 0,
        };
        c.put(k0, vec![1u8; 4]);
        c.put(k1, vec![2u8; 4]);
        c.put(other, vec![3u8; 4]);
        let inv = c.invalidate_file(1, 9);
        assert_eq!(inv.ram_dropped, 2);
        assert!(inv.is_complete());
        assert!(c.get(&k0).is_none());
        assert!(c.get(&k1).is_none());
        assert_eq!(&c.get(&other).unwrap()[..], &[3u8; 4][..]);
    }

    /// A private directory per test. Two disk-tier tests sharing one would delete
    /// each other's `.blk` files and the failure would read as a cache bug.
    fn disk_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vfs-cache-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn blk_names(dir: &std::path::Path) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut v: Vec<String> = rd
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".blk"))
            .collect();
        v.sort();
        v
    }

    /// **The unit-level statement of the corruption.** A block that has been
    /// evicted from RAM is still servable from disk, so invalidation has to reach
    /// it. Enumerating the RAM map — which is what this did — cannot, because
    /// eviction is precisely the act of removing the key from that map.
    #[test]
    fn invalidate_file_drops_a_block_that_eviction_left_only_on_disk() {
        let dir = disk_dir("inv-evicted");
        let c = BlockCache::new(CacheConfig {
            block_size: 64,
            ram_budget: 64, // room for exactly one block
            disk_dir: Some(dir.clone()),
        });
        assert_eq!(c.shard_count(), 1);
        let evicted = BlockKey {
            source_id: 1,
            file_id: 9,
            block_index: 0,
        };
        let resident = BlockKey {
            source_id: 1,
            file_id: 9,
            block_index: 1,
        };
        c.put(evicted, vec![b'A'; 64]);
        c.put(resident, vec![b'B'; 64]);
        assert!(
            c.stats().ram_evicts >= 1,
            "the fixture did not evict, so nothing here is about eviction"
        );
        assert_eq!(blk_names(&dir).len(), 2, "both blocks should be on disk");

        let inv = c.invalidate_file(1, 9);
        assert!(inv.is_complete(), "{inv:?}");
        assert_eq!(inv.disk_dropped, 2, "both .blk files must go, not just the resident one");
        assert!(
            blk_names(&dir).is_empty(),
            "left behind on disk: {:?}",
            blk_names(&dir)
        );
        assert!(
            c.get(&evicted).is_none(),
            "the evicted block was re-served from disk after invalidation"
        );
        assert!(c.get(&resident).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The disk tier outlives the process, so it can hold `.blk` files whose block
    /// indices this run's block size would never produce. Invalidation must clear
    /// them too — they are stale for exactly the same reason — which is why the
    /// sweep matches a filename prefix instead of a computed index range.
    ///
    /// It must also stop at the file boundary: file id `0x20`'s blocks are not
    /// file id `0x2`'s, even though the shorter hex string starts the longer one.
    #[test]
    fn invalidate_file_clears_disk_blocks_from_a_run_with_a_different_block_size() {
        let dir = disk_dir("inv-stale-run");
        std::fs::create_dir_all(&dir).unwrap();
        // A previous run with a much smaller block size: indices far past
        // anything this run would generate, and payloads of its own size.
        for i in [0u64, 1, 7, 4095, 1_000_000] {
            std::fs::write(dir.join(format!("1_2_{i:x}.blk")), vec![b'X'; 8]).unwrap();
        }
        // Neighbours that must survive: another file whose id merely *starts* with
        // the same hex digits, and another source id.
        std::fs::write(dir.join("1_20_0.blk"), b"keep-a").unwrap();
        std::fs::write(dir.join("2_2_0.blk"), b"keep-b").unwrap();
        // A file this cache did not write at all.
        std::fs::write(dir.join("notes.txt"), b"keep-c").unwrap();

        let c = BlockCache::new(CacheConfig {
            block_size: 1 << 20,
            ram_budget: 1 << 20,
            disk_dir: Some(dir.clone()),
        });
        let inv = c.invalidate_file(1, 2);
        assert!(inv.is_complete(), "{inv:?}");
        assert_eq!(inv.disk_dropped, 5, "every stale index must go: {inv:?}");
        assert_eq!(inv.ram_dropped, 0, "nothing was resident");
        assert_eq!(blk_names(&dir), vec!["1_20_0.blk", "2_2_0.blk"]);
        assert!(dir.join("notes.txt").exists(), "an unrelated file was deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Invalidation that cannot finish must say so rather than report success. A
    /// directory sitting where a `.blk` file belongs is not removable by
    /// `remove_file`, which is a faithful stand-in for the real cases (a locked
    /// file, a revoked ACL) without needing either.
    #[test]
    fn invalidation_that_cannot_remove_a_disk_block_reports_failure() {
        let dir = disk_dir("inv-unremovable");
        std::fs::create_dir_all(dir.join("1_2_0.blk")).unwrap();
        let c = BlockCache::new(CacheConfig {
            block_size: 64,
            ram_budget: 1024,
            disk_dir: Some(dir.clone()),
        });
        let inv = c.invalidate_file(1, 2);
        assert!(
            !inv.is_complete(),
            "an unremovable block was reported as fully invalidated: {inv:?}"
        );
        assert_eq!(inv.failures, 1);
        assert_eq!(
            c.stats().invalidate_failures,
            1,
            "the failure must also be visible to anyone reading stats"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **The silent-no-cache defect.** A block bigger than a shard's budget is
    /// refused by `put`, which returns nothing, so before this the cache could be
    /// completely disabled with no counter, no error and no log line to show it.
    /// The configuration here is the measured one: 4 KiB `block_size` and a 64 MiB
    /// budget give 64 shards with 1 MiB each, and a provider hinting 4 MiB stores
    /// 4 MiB blocks into them.
    #[test]
    fn a_block_too_large_for_a_shard_is_counted_not_silently_dropped() {
        let c = BlockCache::new(CacheConfig {
            block_size: 4096,
            ram_budget: 64 * 1024 * 1024,
            disk_dir: None,
        });
        assert_eq!(c.shard_count(), MAX_SHARDS);
        assert_eq!(c.max_cacheable_block(), 1 << 20, "64 MiB across 64 shards");
        c.put(k(0), vec![0u8; 4 << 20]);
        let s = c.stats();
        assert_eq!(s.ram_blocks, 0, "it does not fit, so it is not resident");
        assert_eq!(
            s.oversized_rejects, 1,
            "a put that cached nothing must be visible somewhere"
        );
        // And the same put through a cache that can hold it is not flagged.
        let ok = BlockCache::new(CacheConfig {
            block_size: 4 << 20,
            ram_budget: 64 * 1024 * 1024,
            disk_dir: None,
        });
        ok.put(k(0), vec![0u8; 4 << 20]);
        assert_eq!(ok.stats().oversized_rejects, 0);
        assert_eq!(ok.stats().ram_blocks, 1);
    }

    fn k(i: u64) -> BlockKey {
        BlockKey {
            source_id: 3,
            file_id: 4,
            block_index: i,
        }
    }

    /// Sharding must not shrink a small cache to nothing. A budget too small to
    /// give every shard `MIN_BLOCKS_PER_SHARD` blocks collapses to one shard,
    /// which behaves exactly like the unsharded original — this is what keeps
    /// the small-fixture configurations above working unchanged.
    #[test]
    fn shard_count_follows_the_budget_and_collapses_when_it_is_small() {
        let mk = |block_size, ram_budget| {
            BlockCache::new(CacheConfig {
                block_size,
                ram_budget,
                disk_dir: None,
            })
        };
        assert_eq!(mk(8, 16).shard_count(), 1, "budget for 2 blocks: 1 shard");
        assert_eq!(mk(16, 0).shard_count(), 1, "zero budget: 1 shard");
        assert_eq!(
            mk(1 << 20, 64 << 20).shard_count(),
            8,
            "the default config: 64 blocks / 8 per shard"
        );
        assert_eq!(
            mk(4096, 1 << 30).shard_count(),
            MAX_SHARDS,
            "a large budget is capped at MAX_SHARDS"
        );
        // Rounding down, not up: 12 shards' worth of capacity gives 8, because 16
        // would put fewer than MIN_BLOCKS_PER_SHARD blocks in each.
        let c = mk(4096, 4096 * 8 * 12);
        assert_eq!(c.shard_count(), 8);
        assert!(
            c.shard_count() as u64 * MIN_BLOCKS_PER_SHARD * 4096 <= 4096 * 8 * 12,
            "every shard must be able to hold MIN_BLOCKS_PER_SHARD blocks"
        );
    }

    /// Keys must spread across shards. A hash that preserved low bits would put
    /// a sequential run of `block_index` values on one shard and quietly undo
    /// the sharding — the cache would still be correct and still serialise.
    #[test]
    fn sequential_block_indices_spread_across_shards() {
        let c = BlockCache::new(CacheConfig {
            block_size: 4096,
            ram_budget: 1 << 30,
            disk_dir: None,
        });
        assert_eq!(c.shard_count(), MAX_SHARDS);
        let mut seen = std::collections::HashSet::new();
        for i in 0..32u64 {
            seen.insert(c.shard(&k(i)) as *const _ as usize);
        }
        assert!(
            seen.len() >= 16,
            "32 consecutive block indices landed on only {} of {MAX_SHARDS} shards",
            seen.len()
        );
    }

    /// CLOCK's second chance: a block that has been hit survives one eviction
    /// pass in preference to one that has not. This is the property that keeps
    /// the ordering-free hit path from degrading eviction into FIFO.
    #[test]
    fn eviction_gives_a_hit_block_a_second_chance() {
        // One shard (small budget), room for exactly 2 of these blocks.
        let c = BlockCache::new(CacheConfig {
            block_size: 64,
            ram_budget: 128,
            disk_dir: None,
        });
        assert_eq!(c.shard_count(), 1);
        c.put(k(0), vec![0u8; 64]);
        c.put(k(1), vec![1u8; 64]);
        // Touch block 0, so it is referenced and block 1 is not. Block 0 is also
        // the *older* insertion, so plain FIFO would evict it — the assertion
        // below only holds if the reference bit is consulted.
        assert!(c.get(&k(0)).is_some());
        c.put(k(2), vec![2u8; 64]);
        assert!(
            c.get(&k(0)).is_some(),
            "the referenced block was evicted; the second chance is not working"
        );
        assert!(c.stats().ram_bytes <= 128);
    }

    /// Re-inserting a key must leave the map and the CLOCK ring in 1:1
    /// correspondence. A duplicate ring entry would let the sweep evict a live
    /// block early; a missing one would make a block permanently un-evictable
    /// and leak the budget. Neither is visible without forcing eviction after a
    /// replace, which is what this does.
    #[test]
    fn replacing_a_key_keeps_the_ring_consistent() {
        let c = BlockCache::new(CacheConfig {
            block_size: 64,
            ram_budget: 256,
            disk_dir: None,
        });
        assert_eq!(c.shard_count(), 1);
        for _ in 0..5 {
            c.put(k(0), vec![9u8; 64]);
        }
        assert_eq!(c.stats().ram_blocks, 1, "replace must not duplicate");
        assert_eq!(c.stats().ram_bytes, 64, "replace must not double-count");
        // Now push far past the budget. If the ring had lost k(0)'s entry, k(0)
        // would be un-evictable and `ram_bytes` would drift above the budget.
        for i in 1..20 {
            c.put(k(i), vec![(i % 251) as u8; 64]);
        }
        let s = c.stats();
        assert!(s.ram_bytes <= 256, "budget exceeded: {} bytes", s.ram_bytes);
        assert_eq!(
            s.ram_bytes,
            s.ram_blocks * 64,
            "byte accounting drifted from block count"
        );
        assert!(s.ram_evicts >= 1);
    }

    /// The total budget is respected across shards, not just within one.
    #[test]
    fn ram_bytes_stays_within_the_global_budget_when_sharded() {
        let c = BlockCache::new(CacheConfig {
            block_size: 4096,
            ram_budget: 4096 * 8 * 4, // 4 shards, 8 blocks each
            disk_dir: None,
        });
        assert_eq!(c.shard_count(), 4);
        for i in 0..500u64 {
            c.put(k(i), vec![0u8; 4096]);
        }
        let s = c.stats();
        assert!(
            s.ram_bytes <= 4096 * 8 * 4,
            "{} bytes resident against a {} byte budget",
            s.ram_bytes,
            4096 * 8 * 4
        );
        assert!(s.ram_evicts > 0);
        assert!(s.ram_blocks > 0, "sharding evicted everything");
    }

    /// A hit hands back a handle to the *same* allocation, not a copy of it.
    /// This is the unit-level statement of what `tests/hit_copy_cost.rs`
    /// measures with an allocator, and it is worth having both: this one pins
    /// the mechanism, that one pins the cost.
    #[test]
    fn two_hits_share_one_allocation() {
        let c = BlockCache::new(CacheConfig {
            block_size: 4096,
            ram_budget: 1 << 20,
            disk_dir: None,
        });
        c.put(k(0), vec![7u8; 4096]);
        let a = c.get(&k(0)).unwrap();
        let b = c.get(&k(0)).unwrap();
        assert!(
            std::ptr::eq(a.as_ptr(), b.as_ptr()),
            "hits returned different allocations — the payload is being copied"
        );
    }

    #[test]
    fn disk_tier_roundtrip() {
        let dir = std::env::temp_dir().join(format!("vfs-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let c = BlockCache::new(CacheConfig {
            block_size: 16,
            ram_budget: 0, // force disk-only after put fails RAM
            disk_dir: Some(dir.clone()),
        });
        // With ram_budget 0, put still writes disk then skips RAM.
        let key = BlockKey {
            source_id: 9,
            file_id: 1,
            block_index: 3,
        };
        c.put(key, b"disk-bytes-here!".to_vec());
        // Clear any RAM (none) and read from disk.
        let got = c.get(&key).expect("disk hit");
        assert_eq!(&got[..], b"disk-bytes-here!");
        assert!(c.stats().disk_hits >= 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
