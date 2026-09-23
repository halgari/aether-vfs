//! A deduplicating, compressing block store for caching virtual file system content.
//!
//! Files are identified by caller-chosen byte strings and stored as fixed-size blocks.
//! Blocks are deduplicated by content (BLAKE3-128), compressed with zstd, and appended
//! to a small number of large pack files. Metadata lives in a redb database.

mod codec;
mod config;
mod crash;
mod error;
mod files;
mod index;
mod manifest;

pub use config::{CompactOptions, StoreConfig};
pub use error::{Error, Result};
