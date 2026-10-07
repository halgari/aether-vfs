//! `vfs-storage`: the block store as pull-through cache and layer storage.
//!
//! A [`Storage`] owns one `vfs_block_store::BlockStore`, a redb catalog
//! beside it that names what the store holds (layers and their entries, and
//! cached files' eviction bookkeeping), and a RAM tier of decompressed
//! blocks. See `docs/superpowers/specs/2026-09-29-vfs-storage-design.md`.
//!
//! **Durability.** Layer writes and namespace changes commit non-durably;
//! a *durable point* (store fsync + durable index commit, then the catalog's
//! durable commit) publishes them. [`StorageConfig::durability`] chooses when
//! one runs: by default ([`Durability::Deferred`]) at most every five minutes
//! (or 10,000 catalog commits) while layers change, at once after a rewrite in
//! place of a file that was already durable, and at [`Storage::sync`],
//! [`Storage::close`] and a layer provider's drop; [`Durability::OnEveryClose`]
//! runs one at every close, flush and namespace change. A crash loses at most
//! the changes since the last durable point, and the store always reopens
//! consistent.

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
pub use config::{Durability, ScratchDir, StorageConfig};
pub use evict::ClearReport;
pub use manage::{LayerInfo, SpaceUsage, StorageStats};
pub use reconcile::ReconcileReport;
pub use storage::{CloseOutcome, Storage, StorageError};
// The block store's compression and accounting types, so a host configures
// and reads them without depending on `vfs-block-store` itself.
pub use vfs_block_store::{
    with_write_class, BulkCompression, ClassWriteStats, StoreConfig, Usage, WriteClass, WriteStats,
};
#[cfg(feature = "gpu-zstd")]
pub use vfs_block_store::{GpuConfig, GpuLevel, GpuStats, GPU_MAX_BLOCK};
