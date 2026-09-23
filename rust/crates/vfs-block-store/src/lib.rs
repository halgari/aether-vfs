//! A deduplicating, compressing block store for caching virtual file system content.
//!
//! Files are identified by caller-chosen byte strings and stored as fixed-size blocks.
//! Blocks are deduplicated by content (BLAKE3-128), compressed with zstd, and appended
//! to a small number of large pack files. Metadata lives in a redb database.

mod codec;
mod compact;
mod config;
mod crash;
mod error;
mod files;
mod index;
mod manifest;
mod pack;
mod read;
mod stats;
mod store;
mod tracker;
mod verify;
mod write;

pub use compact::CompactReport;
pub use config::{CompactOptions, StoreConfig};
pub use error::{Error, Result};
pub use stats::{IndexSize, PackStats, Stats};
pub use store::{BlockStore, FileInfo, ReadResult};
pub use verify::VerifyReport;
