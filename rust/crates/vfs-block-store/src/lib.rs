//! A deduplicating, compressing block store for caching virtual file system content.
//!
//! Files are identified by caller-chosen byte strings and stored as fixed-size blocks.
//! Blocks are deduplicated by content (BLAKE3-128), compressed with zstd, and appended
//! to a small number of large pack files. Metadata lives in a redb database.

mod class;
mod codec;
mod compact;
mod compress;
mod config;
mod crash;
mod error;
mod files;
#[cfg(feature = "gpu-zstd")]
mod gpu;
#[cfg(all(test, feature = "gpu-zstd"))]
mod gpu_store_tests;
mod index;
mod manifest;
mod pack;
mod read;
mod stats;
mod store;
mod tracker;
mod verify;
mod write;

pub use class::{with_write_class, WriteClass};
pub use compact::CompactReport;
pub use config::{BulkCompression, CompactOptions, StoreConfig};
pub use error::{Error, Result};
#[cfg(feature = "gpu-zstd")]
pub use gpu::{GpuConfig, GpuLevel, GpuStats, GPU_MAX_BLOCK};
pub use index::IndexStats;
pub use stats::{ClassWriteStats, IndexSize, PackStats, Stats, Usage, WriteStats};
pub use store::{BlockStore, FileInfo, ReadResult};
pub use verify::VerifyReport;
