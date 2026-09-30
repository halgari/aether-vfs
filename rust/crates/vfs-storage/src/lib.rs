//! `vfs-storage`: the block store as pull-through cache and layer storage.
//!
//! A [`Storage`] owns one `vfs_block_store::BlockStore`, a redb [`Catalog`]
//! beside it that names what the store holds (layers and their entries, and
//! cached files' eviction bookkeeping), and a [`RamTier`] of decompressed
//! blocks. See `docs/superpowers/specs/2026-09-29-vfs-storage-design.md`.
//!
//! **Durability.** Layer writes and namespace changes commit non-durably;
//! a *durable point* (store fsync + durable index commit, then the catalog's
//! durable commit) publishes them. [`StorageConfig::durability`] chooses when
//! one runs: by default ([`Durability::Deferred`]) at most every five minutes
//! while layers change, plus at [`Storage::sync`], [`Storage::close`] and a
//! layer provider's drop; [`Durability::OnEveryClose`] runs one at every
//! close, flush and namespace change. A crash loses at most the changes since
//! the last durable point, and the store always reopens consistent.

mod cached;
mod catalog;
mod config;
mod evict;
mod ids;
mod layer;
mod layer_io;
mod manage;
mod ram;
mod reconcile;
mod storage;
#[cfg(test)]
mod test_util;

pub use cached::{CacheStats, SourceKey};
pub use catalog::{CacheRec, Catalog, EntryRec};
pub use config::{Durability, StorageConfig};
pub use ids::{cache_file_id, classify_store_id, layer_file_id, new_guid, Guid, StoreIdKind};
pub use manage::{LayerInfo, StorageStats};
pub use ram::{RamStats, RamTier};
pub use reconcile::ReconcileReport;
pub use storage::{CloseOutcome, Storage, StorageError};
