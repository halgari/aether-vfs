//! `vfs-storage`: the block store as pull-through cache and layer storage.
//!
//! A [`Storage`] owns one `vfs_block_store::BlockStore`, a redb [`Catalog`]
//! beside it that names what the store holds (layers and their entries, and
//! cached files' eviction bookkeeping), and a [`RamTier`] of decompressed
//! blocks. See `docs/superpowers/specs/2026-09-29-vfs-storage-design.md`.

mod cached;
mod catalog;
mod config;
mod evict;
mod ids;
mod ram;
mod storage;

pub use cached::{CacheStats, SourceKey};
pub use catalog::{CacheRec, Catalog, EntryRec};
pub use config::StorageConfig;
pub use ids::{cache_file_id, classify_store_id, layer_file_id, new_guid, Guid, StoreIdKind};
pub use ram::{RamStats, RamTier};
pub use storage::{Storage, StorageError};
