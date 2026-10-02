//! The RAM tier: decompressed blocks, sharded, evicted by CLOCK.
//!
//! Ported from `vfs-cache`'s `BlockCache` (its RAM half; the `.blk` disk tier
//! is gone, since the block store is the disk tier now). Keys are
//! `(store file id, block index)`; see [`crate::ids`] for the ids.
//!
//! ## The three costs this design exists to avoid
//!
//! An earlier version of the original was correct and unusably slow: a 4 KiB
//! read through it ran at **24 MiB/s against ~1400 MiB/s raw**. The causes, and
//! what replaced each:
//!
//! 1. **A hit cloned the whole block.** Blocks are `Arc<[u8]>` and a hit is a
//!    refcount bump; the caller copies only the range it asked for, **after
//!    the lock is released**.
//! 2. **A hit scanned the LRU ordering.** Replaced by **CLOCK** (second
//!    chance): a hit sets one reference bit and never touches the ordering, so
//!    it is O(1) exactly. Eviction sweeps a hand over the ring and is amortised
//!    O(1), since a block can only earn a second chance by having been hit.
//! 3. **One process-wide `Mutex` serialised every reader.** The reference bit
//!    is an `AtomicBool` inside the entry, so **a hit needs only a shared
//!    lock**, and the table is **sharded** so misses (which need exclusive
//!    access) contend only with misses on the same shard.
//!
//! A fourth cost only became visible once the lock stopped hiding it: **the
//! tier's own hit counters.** Process-wide `AtomicU64` increments per hit held
//! 4-thread scaling to 1.35x; they are per-shard. See `Shard::hits`.
//!
//! CLOCK is an LRU approximation. That is the deliberate trade: it is what
//! makes a hit lock-shared and ordering-free.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// A cached block payload. Cloning is a refcount bump, never a copy.
type Block = Arc<[u8]>;

/// A zero-filled block of `len` bytes with one owner, for a caller to decode
/// into (through `Arc::get_mut`) and then [`RamTier::put`]: one allocation,
/// and no copy of the block on its way into the tier.
pub(crate) fn zeroed_block(len: usize) -> Block {
    // An exact-size iterator collects into one allocation, filled in place.
    std::iter::repeat_n(0u8, len).collect()
}

/// Store file id + block index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Key {
    file_id: [u8; 17],
    block: u64,
}

/// Counters of a [`RamTier`], summed over its shards.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RamStats {
    pub hits: u64,
    pub misses: u64,
    pub evicts: u64,
    /// Payload bytes resident.
    pub bytes: u64,
    /// Blocks resident.
    pub blocks: u64,
    /// Blocks [`RamTier::put`] refused because one would not fit a shard on
    /// its own. **Non-zero means the tier is not caching blocks of that size**:
    /// every read of one goes past it. See [`RamTier::max_cacheable_block`].
    pub oversized_rejects: u64,
}

struct RamEntry {
    data: Block,
    /// CLOCK reference bit. Set by a hit through a *shared* borrow — this being
    /// an atomic rather than a `bool` is what lets the hit path take a read lock.
    referenced: AtomicBool,
}

/// One shard: a map, a CLOCK ring, and its own hit counter.
///
/// `map` and `ring` are kept in exact 1:1 correspondence — every key in the map
/// appears in the ring exactly once and vice versa — so the eviction sweep never
/// has to reason about stale ring entries. Every mutation below preserves that.
///
/// Aligned to a cache line so two shards never share one.
#[repr(align(64))]
struct Shard {
    map: HashMap<Key, RamEntry>,
    ring: VecDeque<Key>,
    bytes: u64,
    /// Hits are counted **per shard**: process-wide atomics here were measured
    /// as the largest remaining scalability limit once the lock was fixed (4-thread
    /// scaling 1.35x, 2.1x with the counter moved here). Misses and evictions
    /// stay global: each is followed by real work that dwarfs the increment.
    hits: AtomicU64,
}

/// Never shard so finely that a shard cannot hold a working set.
const MIN_BLOCKS_PER_SHARD: u64 = 8;
/// Upper bound on shards. A constant rather than a function of the core count,
/// so shard geometry is identical on every machine and can be asserted on.
const MAX_SHARDS: usize = 64;
/// The block size [`RamTier::new`] sizes shards for: the block store's default.
const DEFAULT_GEOMETRY_BLOCK: u64 = 64 * 1024;

/// Thread-safe RAM tier of decompressed blocks.
pub struct RamTier {
    budget: u64,
    shards: Box<[RwLock<Shard>]>,
    /// `shards.len() - 1`; `shards.len()` is always a power of two.
    shard_mask: usize,
    /// Per-shard byte budget. Sums to at most `budget`, so resident bytes never
    /// exceed it.
    shard_budget: u64,
    misses: AtomicU64,
    evicts: AtomicU64,
    oversized_rejects: AtomicU64,
    /// The oversized warning is logged once: the condition is static.
    warned_oversized: AtomicBool,
}

impl RamTier {
    /// A tier of `budget_bytes`, with shards sized for the block store's default
    /// block size. A zero budget is "no RAM tier": every `put` is refused.
    pub fn new(budget_bytes: u64) -> Self {
        Self::with_geometry(budget_bytes, DEFAULT_GEOMETRY_BLOCK)
    }

    /// A tier of `budget_bytes` whose shard count is chosen for blocks of
    /// `block_size` bytes: enough shards to spread misses, never so many that a
    /// shard holds fewer than `MIN_BLOCKS_PER_SHARD` of them.
    pub(crate) fn with_geometry(budget_bytes: u64, block_size: u64) -> Self {
        let capacity_shards = budget_bytes / block_size.max(1) / MIN_BLOCKS_PER_SHARD;
        // Largest power of two <= both bounds. Rounding *down* matters: rounding
        // up would push a shard below MIN_BLOCKS_PER_SHARD.
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
                })
            })
            .collect();
        Self {
            budget: budget_bytes,
            shard_mask: n - 1,
            shard_budget: budget_bytes / n as u64,
            shards: shards.into_boxed_slice(),
            misses: AtomicU64::new(0),
            evicts: AtomicU64::new(0),
            oversized_rejects: AtomicU64::new(0),
            warned_oversized: AtomicBool::new(false),
        }
    }

    /// The largest block `put` accepts: the **per-shard** budget. Zero means the
    /// tier is off.
    pub fn max_cacheable_block(&self) -> u64 {
        self.shard_budget
    }

    #[cfg(test)]
    fn shard_count(&self) -> usize {
        self.shards.len()
    }

    fn shard(&self, file_id: &[u8; 17], block: u64) -> &RwLock<Shard> {
        // splitmix64 finalizer over the key. Consecutive block indices must land
        // on different shards — a sequential sweep is the common access pattern,
        // and a low-bit-preserving hash would put a run of blocks on one shard.
        let a = u64::from_le_bytes(file_id[0..8].try_into().unwrap());
        let b = u64::from_le_bytes(file_id[8..16].try_into().unwrap());
        let mut h =
            a ^ b.rotate_left(21) ^ u64::from(file_id[16]).rotate_left(7) ^ block.rotate_left(43);
        h ^= h >> 30;
        h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
        h ^= h >> 27;
        h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
        h ^= h >> 31;
        &self.shards[(h as usize) & self.shard_mask]
    }

    pub fn stats(&self) -> RamStats {
        let mut s = RamStats {
            misses: self.misses.load(Ordering::Relaxed),
            evicts: self.evicts.load(Ordering::Relaxed),
            oversized_rejects: self.oversized_rejects.load(Ordering::Relaxed),
            ..RamStats::default()
        };
        for sh in self.shards.iter() {
            let g = read(sh);
            s.bytes += g.bytes;
            s.blocks += g.map.len() as u64;
            s.hits += g.hits.load(Ordering::Relaxed);
        }
        s
    }

    /// Looks up a block. A hit takes a **shared** lock, sets the CLOCK
    /// reference bit and returns a refcounted handle — no allocation, no copy,
    /// no touch of the eviction ordering.
    pub fn get(&self, file_id: &[u8; 17], block: u64) -> Option<Arc<[u8]>> {
        let key = Key {
            file_id: *file_id,
            block,
        };
        let g = read(self.shard(file_id, block));
        match g.map.get(&key) {
            Some(e) => {
                e.referenced.store(true, Ordering::Relaxed);
                g.hits.fetch_add(1, Ordering::Relaxed);
                Some(Block::clone(&e.data))
            }
            None => {
                drop(g);
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Inserts (or replaces) a block. A block larger than a shard's budget is
    /// refused and counted in [`RamStats::oversized_rejects`].
    pub fn put(&self, file_id: &[u8; 17], block: u64, data: Arc<[u8]>) {
        let key = Key {
            file_id: *file_id,
            block,
        };
        let len = data.len() as u64;
        if len > self.shard_budget {
            self.oversized_rejects.fetch_add(1, Ordering::Relaxed);
            self.warn_oversized(len);
            return;
        }
        let mut g = write(self.shard(file_id, block));
        if let Some(old) = g.map.remove(&key) {
            g.bytes = g.bytes.saturating_sub(old.data.len() as u64);
            // O(n) in this shard's ring, deliberately: replacing happens only
            // when two threads miss the same block at once or a block is
            // refilled after invalidation. Keeping the ring free of duplicates is
            // what lets the sweep trust `map.get`. The hit path never gets here.
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

    /// Drops every block of a file. Cold path: a sweep of every shard.
    ///
    /// Infallible: a poisoned shard is cleared through its poison, because a
    /// shard that keeps a stale block after its file was written would serve it.
    ///
    /// **A stale refill can race it.** A reader that missed this tier, read the
    /// block's *old* bytes from the store (or source) before the write, and
    /// `put`s them after this sweep puts a stale block straight back behind it.
    /// The window is a read racing a write on the same file with no ordering
    /// between them, where the reader may legitimately observe either version;
    /// what this guarantees is that a read strictly *after* a completed write
    /// and its invalidation does not see the old bytes. Closing the window
    /// completely needs a per-file epoch checked by `put`, which is the
    /// caller's to add if it needs it (ported from `vfs-cache`'s
    /// `invalidate_file`, where the refill came from the `.blk` disk tier).
    pub fn invalidate_file(&self, file_id: &[u8; 17]) {
        for s in self.shards.iter() {
            let mut g = write(s);
            let before = g.map.len();
            let mut freed = 0u64;
            g.map.retain(|k, e| {
                let keep = &k.file_id != file_id;
                if !keep {
                    freed += e.data.len() as u64;
                }
                keep
            });
            if g.map.len() == before {
                continue;
            }
            g.bytes = g.bytes.saturating_sub(freed);
            // One pass to restore the map/ring 1:1 invariant, not one per key.
            g.ring.retain(|k| &k.file_id != file_id);
        }
    }

    /// CLOCK second-chance sweep: make room for `need` bytes in `g`.
    ///
    /// Terminates because every iteration strictly decreases one of: bytes
    /// resident (an eviction), ring length (a key already gone from the map), or
    /// `chances`, which starts at the ring length — so a ring in which every
    /// entry is referenced clears bits for one lap and then evicts.
    fn evict_to_fit(&self, g: &mut Shard, need: u64) {
        let mut chances = g.ring.len();
        while g.bytes + need > self.shard_budget {
            let Some(cand) = g.ring.pop_front() else {
                break;
            };
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
                self.evicts.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Logs once that the tier is refusing blocks of this size. A zero budget is
    /// "no RAM tier", asked for explicitly, and not worth a word.
    fn warn_oversized(&self, len: u64) {
        if self.shard_budget == 0 || self.warned_oversized.swap(true, Ordering::Relaxed) {
            return;
        }
        tracing::warn!(
            block_bytes = len,
            shard_budget = self.shard_budget,
            ram_tier_bytes = self.budget,
            shards = self.shards.len(),
            "RAM tier is NOT caching blocks this size: one does not fit a shard's \
             budget, so every read of one goes past the tier. Raise ram_tier_bytes \
             or lower the block size."
        );
    }
}

/// Shared lock, through poison: every mutation here leaves a shard usable (the
/// eviction sweep self-heals a ring/map mismatch), so a panic elsewhere must not
/// turn the tier off or leave stale blocks behind.
fn read(s: &RwLock<Shard>) -> RwLockReadGuard<'_, Shard> {
    s.read().unwrap_or_else(|e| e.into_inner())
}

fn write(s: &RwLock<Shard>) -> RwLockWriteGuard<'_, Shard> {
    s.write().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FID: [u8; 17] = [4; 17];

    fn fid(n: u8) -> [u8; 17] {
        [n; 17]
    }

    fn blk(byte: u8, len: usize) -> Arc<[u8]> {
        vec![byte; len].into()
    }

    #[test]
    fn ram_hit_and_miss() {
        let c = RamTier::with_geometry(1024, 64);
        assert!(c.get(&fid(2), 0).is_none());
        c.put(&fid(2), 0, blk(7, 32));
        assert_eq!(&c.get(&fid(2), 0).unwrap()[..], &[7u8; 32][..]);
        let s = c.stats();
        assert_eq!(s.misses, 1);
        assert_eq!(s.hits, 1);
    }

    #[test]
    fn eviction_under_budget() {
        let c = RamTier::with_geometry(16, 8);
        c.put(&fid(0), 0, blk(1, 8));
        c.put(&fid(0), 1, blk(2, 8));
        c.put(&fid(0), 2, blk(3, 8));
        assert!(c.stats().evicts >= 1);
        assert!(c.stats().bytes <= 16);
    }

    #[test]
    fn invalidate_file_drops_blocks() {
        let c = RamTier::with_geometry(1024, 8);
        c.put(&fid(9), 0, blk(1, 4));
        c.put(&fid(9), 1, blk(2, 4));
        c.put(&fid(10), 0, blk(3, 4));
        c.invalidate_file(&fid(9));
        assert!(c.get(&fid(9), 0).is_none());
        assert!(c.get(&fid(9), 1).is_none());
        assert_eq!(&c.get(&fid(10), 0).unwrap()[..], &[3u8; 4][..]);
        assert_eq!(c.stats().bytes, 4, "the dropped blocks' bytes are released");
    }

    /// Two file ids that differ only in their last byte are different files:
    /// invalidating one must leave the other alone.
    #[test]
    fn invalidate_file_matches_the_whole_id() {
        let c = RamTier::with_geometry(1024, 8);
        let a = fid(1);
        let mut b = fid(1);
        b[16] = 2;
        c.put(&a, 0, blk(1, 4));
        c.put(&b, 0, blk(2, 4));
        c.invalidate_file(&a);
        assert!(c.get(&a, 0).is_none());
        assert_eq!(&c.get(&b, 0).unwrap()[..], &[2u8; 4][..]);
    }

    /// **The silent-no-cache defect.** A block bigger than a shard's budget is
    /// refused by `put`, which returns nothing, so without the counter the tier
    /// could be completely disabled with nothing to show it. 4 KiB geometry and a
    /// 64 MiB budget give 64 shards with 1 MiB each; a 4 MiB block cannot fit.
    #[test]
    fn a_block_too_large_for_a_shard_is_counted_not_silently_dropped() {
        let c = RamTier::with_geometry(64 * 1024 * 1024, 4096);
        assert_eq!(c.shard_count(), MAX_SHARDS);
        assert_eq!(c.max_cacheable_block(), 1 << 20, "64 MiB across 64 shards");
        c.put(&FID, 0, blk(0, 4 << 20));
        let s = c.stats();
        assert_eq!(s.blocks, 0, "it does not fit, so it is not resident");
        assert_eq!(
            s.oversized_rejects, 1,
            "a put that cached nothing must be visible somewhere"
        );
        // And the same put through a tier that can hold it is not flagged.
        let ok = RamTier::with_geometry(64 * 1024 * 1024, 4 << 20);
        ok.put(&FID, 0, blk(0, 4 << 20));
        assert_eq!(ok.stats().oversized_rejects, 0);
        assert_eq!(ok.stats().blocks, 1);
    }

    /// `new` sizes shards for the block store's default block size, so a default
    /// store's blocks always fit a shard.
    #[test]
    fn new_fits_default_store_blocks() {
        let c = RamTier::new(256 << 20);
        assert_eq!(c.shard_count(), MAX_SHARDS);
        assert!(c.max_cacheable_block() >= 64 * 1024);
        c.put(&FID, 0, blk(1, 64 * 1024));
        assert_eq!(c.stats().oversized_rejects, 0);
        assert!(c.get(&FID, 0).is_some());
    }

    /// Sharding must not shrink a small tier to nothing. A budget too small to
    /// give every shard `MIN_BLOCKS_PER_SHARD` blocks collapses to one shard.
    #[test]
    fn shard_count_follows_the_budget_and_collapses_when_it_is_small() {
        assert_eq!(
            RamTier::with_geometry(16, 8).shard_count(),
            1,
            "budget for 2 blocks: 1 shard"
        );
        assert_eq!(
            RamTier::with_geometry(0, 16).shard_count(),
            1,
            "zero budget: 1 shard"
        );
        assert_eq!(
            RamTier::with_geometry(64 << 20, 1 << 20).shard_count(),
            8,
            "64 blocks / 8 per shard"
        );
        assert_eq!(
            RamTier::with_geometry(1 << 30, 4096).shard_count(),
            MAX_SHARDS,
            "a large budget is capped at MAX_SHARDS"
        );
        // Rounding down, not up: 12 shards' worth of capacity gives 8, because 16
        // would put fewer than MIN_BLOCKS_PER_SHARD blocks in each.
        let c = RamTier::with_geometry(4096 * 8 * 12, 4096);
        assert_eq!(c.shard_count(), 8);
        assert!(
            c.shard_count() as u64 * MIN_BLOCKS_PER_SHARD * 4096 <= 4096 * 8 * 12,
            "every shard must be able to hold MIN_BLOCKS_PER_SHARD blocks"
        );
    }

    /// Keys must spread across shards. A hash that preserved low bits would put
    /// a sequential run of block indices on one shard and quietly undo the
    /// sharding — still correct, and serialised.
    #[test]
    fn sequential_block_indices_spread_across_shards() {
        let c = RamTier::with_geometry(1 << 30, 4096);
        assert_eq!(c.shard_count(), MAX_SHARDS);
        let mut seen = std::collections::HashSet::new();
        for i in 0..32u64 {
            seen.insert(c.shard(&FID, i) as *const _ as usize);
        }
        assert!(
            seen.len() >= 16,
            "32 consecutive block indices landed on only {} of {MAX_SHARDS} shards",
            seen.len()
        );
    }

    /// Different files' block 0 must spread too: many small files each read from
    /// the start is the other common pattern.
    #[test]
    fn different_files_spread_across_shards() {
        let c = RamTier::with_geometry(1 << 30, 4096);
        let mut seen = std::collections::HashSet::new();
        for i in 0..32u8 {
            let mut id = [0u8; 17];
            id[0] = b'C';
            id[1] = i;
            seen.insert(c.shard(&id, 0) as *const _ as usize);
        }
        assert!(
            seen.len() >= 16,
            "32 files' block 0 landed on only {} shards",
            seen.len()
        );
    }

    /// CLOCK's second chance: a block that has been hit survives one eviction
    /// pass in preference to one that has not.
    #[test]
    fn eviction_gives_a_hit_block_a_second_chance() {
        // One shard (small budget), room for exactly 2 of these blocks.
        let c = RamTier::with_geometry(128, 64);
        assert_eq!(c.shard_count(), 1);
        c.put(&FID, 0, blk(0, 64));
        c.put(&FID, 1, blk(1, 64));
        // Touch block 0, so it is referenced and block 1 is not. Block 0 is also
        // the *older* insertion, so plain FIFO would evict it.
        assert!(c.get(&FID, 0).is_some());
        c.put(&FID, 2, blk(2, 64));
        assert!(
            c.get(&FID, 0).is_some(),
            "the referenced block was evicted; the second chance is not working"
        );
        assert!(c.stats().bytes <= 128);
    }

    /// Re-inserting a key must leave the map and the CLOCK ring in 1:1
    /// correspondence. A duplicate ring entry would let the sweep evict a live
    /// block early; a missing one would make a block un-evictable and leak the
    /// budget.
    #[test]
    fn replacing_a_key_keeps_the_ring_consistent() {
        let c = RamTier::with_geometry(256, 64);
        assert_eq!(c.shard_count(), 1);
        for _ in 0..5 {
            c.put(&FID, 0, blk(9, 64));
        }
        assert_eq!(c.stats().blocks, 1, "replace must not duplicate");
        assert_eq!(c.stats().bytes, 64, "replace must not double-count");
        for i in 1..20u64 {
            c.put(&FID, i, blk((i % 251) as u8, 64));
        }
        let s = c.stats();
        assert!(s.bytes <= 256, "budget exceeded: {} bytes", s.bytes);
        assert_eq!(
            s.bytes,
            s.blocks * 64,
            "byte accounting drifted from block count"
        );
        assert!(s.evicts >= 1);
    }

    /// The total budget is respected across shards, not just within one.
    #[test]
    fn bytes_stay_within_the_global_budget_when_sharded() {
        let c = RamTier::with_geometry(4096 * 8 * 4, 4096); // 4 shards, 8 blocks each
        assert_eq!(c.shard_count(), 4);
        for i in 0..500u64 {
            c.put(&FID, i, blk(0, 4096));
        }
        let s = c.stats();
        assert!(
            s.bytes <= 4096 * 8 * 4,
            "{} bytes resident against a {} byte budget",
            s.bytes,
            4096 * 8 * 4
        );
        assert!(s.evicts > 0);
        assert!(s.blocks > 0, "sharding evicted everything");
    }

    /// A hit hands back a handle to the *same* allocation, not a copy of it.
    #[test]
    fn two_hits_share_one_allocation() {
        let c = RamTier::with_geometry(1 << 20, 4096);
        c.put(&FID, 0, blk(7, 4096));
        let a = c.get(&FID, 0).unwrap();
        let b = c.get(&FID, 0).unwrap();
        assert!(
            std::ptr::eq(a.as_ptr(), b.as_ptr()),
            "hits returned different allocations — the payload is being copied"
        );
    }

    /// A zero budget is "no RAM tier": every put is refused, nothing is resident,
    /// and it is not a misconfiguration worth a warning.
    #[test]
    fn zero_budget_holds_nothing() {
        let c = RamTier::new(0);
        c.put(&FID, 0, blk(1, 16));
        assert!(c.get(&FID, 0).is_none());
        assert_eq!(c.stats().bytes, 0);
    }
}
