//! `aether-storage`: the block store as pull-through cache and layer storage.
//!
//! A [`Storage`] owns one `aether_block_store::BlockStore`, a redb catalog
//! beside it that names what the store holds (layers and their entries, and
//! cached files' eviction bookkeeping), and a RAM tier of decompressed
//! blocks. See `docs/superpowers/specs/2026-09-29-vfs-storage-design.md`.
//!
//! **Durability.** Layer writes commit non-durably; a *durable point* publishes
//! them. The rules (when one runs, the exceptions, the clean-close skip, lock
//! order, what a crash leaves) are in the `durable` module's docs, which render
//! `DURABILITY.md`.
//!
//! **Registry layers.** A `vfs_embed::Session`'s registry layer taken from
//! [`Storage::layer`] is made durable at session end by the sync hook
//! `registry_sync_for` returns (feature `embed`, on by default; without it
//! this crate does not depend on `vfs-embed`).

mod cached;
mod catalog;
mod config;
mod durable;
mod evict;
mod ids;
mod layer;
mod layer_io;
mod manage;
mod ram;
mod reconcile;
#[cfg(feature = "embed")]
mod registry;
mod storage;
#[cfg(any(test, feature = "test-hooks"))]
mod test_util;
mod util;

pub use cached::{CacheStats, SourceKey};
pub use config::{Durability, ScratchDir, StorageConfig};
pub use evict::ClearReport;
pub use manage::{LayerInfo, SpaceUsage, StorageStats};
pub use reconcile::ReconcileReport;
#[cfg(feature = "embed")]
pub use registry::registry_sync_for;
pub use storage::{CloseOutcome, Storage, StorageError};
#[cfg(all(feature = "test-hooks", not(windows)))]
pub use test_util::snapshot_as_killed;
// The block store's compression and accounting types, so a host configures
// and reads them without depending on `aether-block-store` itself.
pub use aether_block_store::{
    BulkCompression, ClassWriteStats, StoreConfig, Usage, WriteClass, WriteStats, with_write_class,
};
#[cfg(feature = "gpu-zstd")]
pub use aether_block_store::{GPU_MAX_BLOCK, GpuConfig, GpuLevel, GpuStats};
