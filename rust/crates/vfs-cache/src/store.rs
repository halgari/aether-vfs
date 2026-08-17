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
        }
    }

    pub fn block_size(&self) -> u64 {
        self.cfg.block_size.max(1)
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

    /// Drop all blocks for a file (invalidation).
    pub fn invalidate_file(&self, source_id: u64, file_id: u64) {
        // Cold path: a whole-file sweep of every shard. O(resident) by nature —
        // it has to find the file's blocks — and it is not the hit path.
        for s in self.shards.iter() {
            let Ok(mut g) = s.write() else { continue };
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
                }
                if let Some(path) = self.disk_path(k) {
                    let _ = std::fs::remove_file(path);
                }
            }
            // One pass to restore the map/ring 1:1 invariant, not one per key.
            g.ring
                .retain(|k| !(k.source_id == source_id && k.file_id == file_id));
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
        // A block that cannot fit in a shard on its own is not cacheable. With
        // more than one shard this is unreachable (a shard holds at least
        // MIN_BLOCKS_PER_SHARD blocks); with one shard it is the original
        // `len <= ram_budget` check, unchanged.
        if len > self.shard_budget {
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

    fn disk_path(&self, key: &BlockKey) -> Option<PathBuf> {
        let dir = self.cfg.disk_dir.as_ref()?;
        Some(dir.join(format!(
            "{:x}_{:x}_{:x}.blk",
            key.source_id, key.file_id, key.block_index
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
        c.invalidate_file(1, 9);
        assert!(c.get(&k0).is_none());
        assert!(c.get(&k1).is_none());
        assert_eq!(&c.get(&other).unwrap()[..], &[3u8; 4][..]);
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
