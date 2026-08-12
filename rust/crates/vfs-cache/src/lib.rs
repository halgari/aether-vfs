//! Fixed-size block cache with a RAM LRU tier and optional on-disk tier.
//!
//! Key: `(source_id, file_id, block_index)`. Reads are block-aligned; the
//! [`CachingBackend`] wrapper fills whole blocks from the inner source and
//! slices for the caller.

mod backend;
mod store;

pub use backend::CachingBackend;
pub use store::{BlockCache, BlockKey, CacheConfig, CacheStats};

/// Default block size: 1 MiB.
pub const DEFAULT_BLOCK_SIZE: u64 = 1024 * 1024;
