//! Fixed-size block cache with a sharded RAM tier and optional on-disk tier.
//!
//! Key: `(source_id, file_id, block_index)`. Reads are block-aligned; the
//! [`CachingProvider`] wrapper fills whole blocks from the inner source and
//! hands the caller a refcounted [`Block`] to slice from — a hit copies only the
//! requested range, and copies it after the cache's lock is released.
//!
//! See [`store`](store)'s module docs for the three hit-path costs this design
//! exists to avoid, and `tests/hit_copy_cost.rs` / `tests/hit_scaling_cost.rs`
//! for the assertions that hold it to them.

mod provider;
mod store;

pub use provider::CachingProvider;
pub use store::{Block, BlockCache, BlockKey, CacheConfig, CacheStats, Invalidation};

/// Default block size: 1 MiB. Used only when the wrapped provider declares no
/// `preferred_block` — a source that states its natural unit gets that instead
/// (see [`CachingProvider::new`]).
///
/// **Why this is still 1 MiB.** A spike measured 64 KiB blocks at 1094 MiB/s
/// against 1 MiB blocks at 24 MiB/s for 4 KiB reads and concluded the default
/// was wrong. The cause was not the block size: it was that a hit cloned the
/// whole block, so per-hit cost was *proportional* to it. That sweep was
/// measuring the clone. With the clone gone a hit is O(1) in block size, the
/// sweep flattens — 2387 MiB/s at 4 KiB blocks against 2646 at 1 MiB, measured
/// on the same harness — so there is no throughput argument left for changing
/// it, and the earlier figure would have been a fit to a term that no longer
/// exists.
///
/// What remains is the real trade, and it does not favour a smaller default:
/// block size is **read amplification against boundary crossings**. This cache
/// exists for `slow` sources, where one large fetch beats sixteen small ones,
/// and a bigger block is what turns 16384 crossings into 64. The size that
/// should vary per source is exactly the one the source knows, which is why
/// `preferred_block` now decides it and this constant is only the fallback for
/// providers that decline to say.
pub const DEFAULT_BLOCK_SIZE: u64 = 1024 * 1024;
