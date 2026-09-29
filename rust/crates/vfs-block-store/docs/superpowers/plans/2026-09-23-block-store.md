# Block Store Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `block-store`, a Rust library that caches virtual file system content as deduplicated, zstd-compressed 64 KiB blocks in a few large pack files, with metadata in redb.

**Architecture:** Block data is appended to pack files (default 4 GiB). A redb index maps `(file_id, segment)` to manifest segments of block ids, maps block ids to pack locations with refcounts, and maps BLAKE3-128 hashes to block ids for dedup. Writes compress in parallel outside any lock and commit one short transaction. Reads use a snapshot plus positioned reads. Compaction copies live records out of mostly-garbage packs and deletes them once older reads finish.

**Tech Stack:** Rust 2024 (MSRV 1.89), redb 4.3, zstd 0.14, blake3 1.8, xxhash-rust 0.8 (xxh3), rayon 1.12, thiserror 2, tracing 0.1; tests use proptest, tempfile, criterion and memory-stats.

**Spec:** `docs/superpowers/specs/2026-09-23-block-store-design.md`. Read it first. Section 14 of the spec lists the amendments made while preparing this plan; where the plan and an older reading of the spec differ, the plan and section 14 win.

**How this plan was verified:** every code block below was compiled and tested before the plan was written. The plan was then replayed task by task in an empty directory. At each task, the new tests failed to compile without the implementation, and passed with it. At the end: 64 tests pass, the scale test is ignored by default, and `cargo clippy --all-targets --all-features -- -D warnings` is clean. Copy the code exactly. If something doesn't compile, check your toolchain against the constraints below before changing code.

## Global Constraints

- **Toolchain:** Rust edition 2024, `rust-version = "1.89"` (needs `File::try_lock`, let-chains, `as_chunks`). Verified with rustc 1.98.
- **Platforms:** Windows, macOS, Linux. No platform-specific dependencies; positional reads use `std::os::unix::fs::FileExt::read_exact_at` or `std::os::windows::fs::FileExt::seek_read`.
- **Runtime dependencies (exact major versions):** `blake3 = "1.8"`, `rayon = "1.12"`, `redb = "4.3"`, `thiserror = "2"`, `tracing = "0.1"`, `xxhash-rust = { version = "0.8", features = ["xxh3"] }`, `zstd = "0.14"`. Add no others.
- **Defaults:** `block_size` 64 KiB (fixed at creation, stored in the index), `zstd_level` 6, `max_pack_size` 4 GiB, `index_cache_bytes` 64 MiB, `write_txn_bytes` 16 MiB, `auto_flush_bytes` 1 GiB, `max_file_id_len` 256.
- **Hashing:** the dedup key is BLAKE3 truncated to 128 bits; the record checksum is xxh3-64 over the stored bytes. The read path never computes BLAKE3.
- **Record header:** exactly 40 bytes, little-endian: magic `SBLK` u32, flags u8, 3 reserved zero bytes, raw_len u32, stored_len u32, hash [16], checksum u64.
- **Process model:** one process per store (exclusive `LOCK` file); `BlockStore` is `Send + Sync`.
- **redb isolation:** no redb type appears outside `src/index.rs`.
- **Locking rule:** `Index::update` closures never take the pack writer lock. The writer lock is held around an index update in exactly two places: `append_records` (registering a new pack) and `durable_commit`.
- **Durability rule:** a durable index commit happens only after the pack bytes it references are fsynced (`durable_commit` syncs, then commits `Immediate`, while holding the writer lock). New packs are registered in the index before their files are created.
- **Style:** `cargo fmt` defaults. Intermediate tasks may show `dead_code` warnings for crate-internal items that later tasks use; the final task requires `clippy -D warnings` to be clean.
- **Test commands** work in both bash and PowerShell. Where an environment variable is set, the PowerShell form is given too.

## File Structure

```
Cargo.toml
README.md
.github/workflows/ci.yml
src/
  lib.rs        module list and public re-exports
  error.rs      Error enum, Result, redb error conversions
  config.rs     StoreConfig (+ validation), CompactOptions
  crash.rs      feature-gated crash points for tests
  codec.rs      record header, BLAKE3-128, xxh3-64, zstd encode/decode
  manifest.rs   manifest segment encoding (pure functions)
  index.rs      redb tables, snapshots, write transactions, refcounting
  files.rs      manifest edits in a transaction (set_slots, resize, remove)
  pack.rs       pack file names, PackWriter, PackFiles (positioned reads)
  tracker.rs    read generation tracking for safe pack deletion
  store.rs      BlockStore: open/recover, flush/close, append_records, stat/set_len/delete/cached_ranges
  stats.rs      stats(), index_size()
  write.rs      write_blocks: validation, dedup, parallel compression, commit
  read.rs       read, read_record, heal
  verify.rs     verify() consistency checker
  compact.rs    compact(), delete_retired()
tests/
  common/mod.rs shared helpers (small 4 KiB-block config, data generators)
  lifecycle.rs  open/lock/config/set_len/delete/flush
  write.rs      write path, dedup, refcount accounting
  read.rs       read path, sparse reads, healing, concurrency
  verify.rs     verify() passes and detects damage
  compact.rs    compaction, concurrency during compaction
  model.rs      proptest model-based test
  crash.rs      subprocess crash tests (feature crash-points)
  scale.rs      opt-in 100 GiB scale test
benches/
  store.rs      criterion benchmarks
```

---

### Task 1: Crate scaffold, errors, configuration, crash points

**Files:**
- Create: `Cargo.toml`
- Create: `src/lib.rs`
- Create: `src/error.rs`
- Create: `src/crash.rs`
- Create: `src/config.rs`

Creates the crate with every runtime dependency, the error type, the configuration and its
validation, and the feature-gated crash-point hook used by the crash tests in Task 14.

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub enum Error { Io(io::Error), Index(Box<dyn std::error::Error + Send + Sync>), NotFound, Unaligned, OutOfRange, FileIdTooLong, Locked, Corrupt(String), Config(String), PartialWrite { blocks_written: u64, source: Box<Error> } }` and `pub type Result<T>`. `From` impls exist for `io::Error` and every redb error type, so `?` works on redb calls.
  - `pub struct StoreConfig { block_size: u32, zstd_level: i32, max_pack_size: u64, index_cache_bytes: usize, write_txn_bytes: usize, auto_flush_bytes: u64, max_file_id_len: usize, compression_threads: Option<usize> }` with `Default` and `pub(crate) fn validate(&self) -> Result<()>`.
  - `pub struct CompactOptions { min_garbage_ratio: f64, max_bytes: u64 }` with `Default` (0.5, `u64::MAX`).
  - `pub fn crash::point(name: &str)`: aborts the process when the `crash-points` feature is on and `BLOCK_STORE_CRASH_AT == name`; a no-op otherwise.

- [ ] **Step 0: Create the crate files that have no tests**

`.gitignore` already contains `/target`. Create `Cargo.toml`:

```toml
[package]
name = "block-store"
version = "0.1.0"
edition = "2024"
rust-version = "1.89"
description = "Deduplicating, compressing block store for a virtual file system cache"
license = "MIT OR Apache-2.0"

[dependencies]
blake3 = "1.8"
rayon = "1.12"
redb = "4.3"
thiserror = "2"
tracing = "0.1"
xxhash-rust = { version = "0.8", features = ["xxh3"] }
zstd = "0.14"

[dev-dependencies]
proptest = "1.11"
tempfile = "3.27"

[features]
# Enables crash::point() calls that abort the process when BLOCK_STORE_CRASH_AT matches.
crash-points = []
```

Create `src/error.rs`:

```rust
use std::io;

/// Errors returned by the block store.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    #[error("index error: {0}")]
    Index(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("file not found")]
    NotFound,
    #[error("write is not block-aligned")]
    Unaligned,
    #[error("block index out of range")]
    OutOfRange,
    #[error("file id is longer than the configured maximum")]
    FileIdTooLong,
    #[error("store is locked by another process")]
    Locked,
    #[error("corrupt store: {0}")]
    Corrupt(String),
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("write failed after {blocks_written} blocks: {source}")]
    PartialWrite {
        blocks_written: u64,
        #[source]
        source: Box<Error>,
    },
}

pub type Result<T> = std::result::Result<T, Error>;

macro_rules! from_redb {
    ($($t:ty),* $(,)?) => {
        $(impl From<$t> for Error {
            fn from(e: $t) -> Self {
                Error::Index(Box::new(e))
            }
        })*
    };
}

from_redb!(
    redb::Error,
    redb::DatabaseError,
    redb::TransactionError,
    redb::TableError,
    redb::StorageError,
    redb::CommitError,
    redb::SetDurabilityError,
);
```

Create `src/crash.rs`:

```rust
//! Crash injection for tests. With the `crash-points` feature, `point(name)` aborts the process
//! when the `BLOCK_STORE_CRASH_AT` environment variable equals `name`. Without it, `point` is a no-op.

#[cfg(feature = "crash-points")]
pub fn point(name: &str) {
    if std::env::var("BLOCK_STORE_CRASH_AT").is_ok_and(|v| v == name) {
        std::process::abort();
    }
}

#[cfg(not(feature = "crash-points"))]
#[inline(always)]
pub fn point(_name: &str) {}
```

Create `src/lib.rs`:

```rust
//! A deduplicating, compressing block store for caching virtual file system content.
//!
//! Files are identified by caller-chosen byte strings and stored as fixed-size blocks.
//! Blocks are deduplicated by content (BLAKE3-128), compressed with zstd, and appended
//! to a small number of large pack files. Metadata lives in a redb database.

mod config;
mod crash;
mod error;

pub use config::{CompactOptions, StoreConfig};
pub use error::{Error, Result};
```

- [ ] **Step 1: Write the failing tests**

Create `src/config.rs` containing only its test module for now (the implementation goes above it in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_valid() {
        let cfg = StoreConfig::default();
        cfg.validate().unwrap();
        assert_eq!(cfg.block_size, 64 * 1024);
        assert_eq!(cfg.zstd_level, 6);
        assert_eq!(cfg.max_pack_size, 4 << 30);
    }

    #[test]
    fn rejects_out_of_range_values() {
        let bad = [
            StoreConfig {
                block_size: 1000,
                ..StoreConfig::default()
            },
            StoreConfig {
                zstd_level: 99,
                ..StoreConfig::default()
            },
            StoreConfig {
                max_pack_size: 1,
                ..StoreConfig::default()
            },
            StoreConfig {
                max_file_id_len: 0,
                ..StoreConfig::default()
            },
        ];
        for cfg in bad {
            assert!(matches!(cfg.validate(), Err(Error::Config(_))), "{cfg:?}");
        }
    }
}
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --lib config`
Expected: FAIL, compile error: `cannot find type StoreConfig in this scope` (the test module exists but the implementation does not).

- [ ] **Step 3: Write the implementation**

Insert this **above** the `#[cfg(test)]` module in `src/config.rs`:

```rust
use crate::error::{Error, Result};

/// Store configuration. `block_size` is fixed when a store is created; the rest may change between opens.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Block size in bytes. Fixed at creation and saved in the index.
    pub block_size: u32,
    /// zstd compression level for new writes.
    pub zstd_level: i32,
    /// A pack is sealed once appending another record would exceed this size.
    pub max_pack_size: u64,
    /// redb page cache size.
    pub index_cache_bytes: usize,
    /// Large writes are split into index transactions of about this many bytes.
    pub write_txn_bytes: usize,
    /// A durable flush happens automatically after this many bytes are appended. Bounds redb memory
    /// use, since non-durable redb commits hold memory until the next durable commit.
    pub auto_flush_bytes: u64,
    /// Maximum length of a file id in bytes.
    pub max_file_id_len: usize,
    /// Threads for hashing and compression. `None` uses rayon's global pool.
    pub compression_threads: Option<usize>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            block_size: 64 * 1024,
            zstd_level: 6,
            max_pack_size: 4 << 30,
            index_cache_bytes: 64 << 20,
            write_txn_bytes: 16 << 20,
            auto_flush_bytes: 1 << 30,
            max_file_id_len: 256,
            compression_threads: None,
        }
    }
}

impl StoreConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        if !(4096..=(16 << 20)).contains(&self.block_size) {
            return Err(Error::Config(
                "block_size must be between 4 KiB and 16 MiB".into(),
            ));
        }
        if !zstd::compression_level_range().contains(&self.zstd_level) {
            return Err(Error::Config("zstd_level out of range".into()));
        }
        if self.max_pack_size < self.block_size as u64 * 2 {
            return Err(Error::Config(
                "max_pack_size must be at least two blocks".into(),
            ));
        }
        if self.max_file_id_len == 0 || self.max_file_id_len > 4096 {
            return Err(Error::Config(
                "max_file_id_len must be between 1 and 4096".into(),
            ));
        }
        Ok(())
    }
}

/// Options for [`crate::BlockStore::compact`].
#[derive(Debug, Clone)]
pub struct CompactOptions {
    /// Only sealed packs whose garbage fraction is at least this are compacted.
    pub min_garbage_ratio: f64,
    /// Stop starting new packs once this many pack bytes have been processed.
    pub max_bytes: u64,
}

impl Default for CompactOptions {
    fn default() -> Self {
        Self {
            min_garbage_ratio: 0.5,
            max_bytes: u64::MAX,
        }
    }
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --lib config`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml src/config.rs src/crash.rs src/error.rs src/lib.rs
git commit -m "feat: crate scaffold with errors, config and crash points"
```

---

### Task 2: Codec: hashing, checksums, compression, record header

**Files:**
- Create: `src/codec.rs`
- Modify: `src/lib.rs` (full new content below)

Everything about a single pack record: the 40-byte header, BLAKE3-128 dedup hashes, xxh3-64
checksums over stored bytes, and zstd compression with a raw fallback when compression does not
help. Compressor and decompressor contexts are cached per thread.

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub const MAGIC: u32`, `pub const HEADER_LEN: usize = 40`, `pub const FLAG_COMPRESSED: u8 = 1`, `pub type Hash128 = [u8; 16]`.
  - `pub fn hash128(data: &[u8]) -> Hash128` and `pub fn checksum(data: &[u8]) -> u64`.
  - `pub struct RecordHeader { flags: u8, raw_len: u32, stored_len: u32, hash: Hash128, checksum: u64 }` with `encode(&self) -> [u8; 40]`, `decode(&[u8]) -> Result<Self, &'static str>` and `record_len(&self) -> u64`.
  - `pub struct EncodedBlock { header: RecordHeader, payload: Vec<u8> }`.
  - `pub fn encode_block(data: &[u8], hash: Hash128, level: i32) -> io::Result<EncodedBlock>`.
  - `pub fn decode_payload(header: &RecordHeader, payload: &[u8], out: &mut [u8]) -> Result<(), &'static str>`: verifies the checksum and writes the uncompressed block; `out.len()` must equal `raw_len`.

- [ ] **Step 1: Write the failing tests**

Create `src/codec.rs` containing only its test module for now (the implementation goes above it in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) -> EncodedBlock {
        let enc = encode_block(data, hash128(data), 6).unwrap();
        let mut out = vec![0u8; data.len()];
        decode_payload(&enc.header, &enc.payload, &mut out).unwrap();
        assert_eq!(out, data);
        enc
    }

    #[test]
    fn compressible_block_is_compressed() {
        let enc = roundtrip(&vec![7u8; 65536]);
        assert_eq!(enc.header.flags, FLAG_COMPRESSED);
        assert!(enc.payload.len() < 1000);
    }

    #[test]
    fn incompressible_block_is_stored_raw() {
        let mut buf = vec![0u8; 65536];
        blake3::Hasher::new()
            .update(b"seed")
            .finalize_xof()
            .fill(&mut buf);
        let enc = roundtrip(&buf);
        assert_eq!(enc.header.flags, 0);
        assert_eq!(enc.payload, buf);
    }

    #[test]
    fn short_block_roundtrips() {
        roundtrip(b"hello world");
        roundtrip(b"");
    }

    #[test]
    fn header_roundtrips() {
        let h = RecordHeader {
            flags: 1,
            raw_len: 65536,
            stored_len: 123,
            hash: [9; 16],
            checksum: 42,
        };
        let bytes = h.encode();
        assert_eq!(RecordHeader::decode(&bytes).unwrap(), h);
        assert_eq!(h.record_len(), 40 + 123);
    }

    #[test]
    fn header_rejects_bad_magic_and_flags() {
        let h = RecordHeader {
            flags: 0,
            raw_len: 1,
            stored_len: 1,
            hash: [0; 16],
            checksum: 0,
        };
        let mut bytes = h.encode();
        bytes[0] ^= 0xff;
        assert_eq!(RecordHeader::decode(&bytes), Err("bad magic"));
        let mut bytes = h.encode();
        bytes[4] = 0x80;
        assert_eq!(RecordHeader::decode(&bytes), Err("unknown flags"));
        assert_eq!(RecordHeader::decode(&bytes[..10]), Err("short header"));
    }

    #[test]
    fn corrupted_payload_is_rejected() {
        let data = vec![3u8; 4096];
        let mut enc = encode_block(&data, hash128(&data), 6).unwrap();
        enc.payload[0] ^= 1;
        let mut out = vec![0u8; data.len()];
        assert_eq!(
            decode_payload(&enc.header, &enc.payload, &mut out),
            Err("checksum mismatch")
        );
    }

    #[test]
    fn hash_is_first_16_bytes_of_blake3() {
        assert_eq!(&hash128(b"abc")[..], &blake3::hash(b"abc").as_bytes()[..16]);
    }
}
```

Replace `src/lib.rs` with:

```rust
//! A deduplicating, compressing block store for caching virtual file system content.
//!
//! Files are identified by caller-chosen byte strings and stored as fixed-size blocks.
//! Blocks are deduplicated by content (BLAKE3-128), compressed with zstd, and appended
//! to a small number of large pack files. Metadata lives in a redb database.

mod codec;
mod config;
mod crash;
mod error;

pub use config::{CompactOptions, StoreConfig};
pub use error::{Error, Result};
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --lib codec`
Expected: FAIL, compile error: unresolved names `encode_block`, `hash128`, `RecordHeader`.

- [ ] **Step 3: Write the implementation**

Insert this **above** the `#[cfg(test)]` module in `src/codec.rs`:

```rust
//! Block encoding: hashing, checksums, compression and the on-disk record header.

use std::cell::RefCell;
use std::io;

/// Record marker, "SBLK" in little-endian byte order.
pub const MAGIC: u32 = u32::from_le_bytes(*b"SBLK");
/// Size of the fixed record header in bytes.
pub const HEADER_LEN: usize = 40;
/// Header flag: payload is a zstd frame (otherwise raw bytes).
pub const FLAG_COMPRESSED: u8 = 1;

/// BLAKE3 hash truncated to 128 bits; the dedup key.
pub type Hash128 = [u8; 16];

pub fn hash128(data: &[u8]) -> Hash128 {
    let mut out = [0u8; 16];
    out.copy_from_slice(&blake3::hash(data).as_bytes()[..16]);
    out
}

/// Integrity checksum over stored (possibly compressed) bytes.
pub fn checksum(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    pub flags: u8,
    pub raw_len: u32,
    pub stored_len: u32,
    pub hash: Hash128,
    pub checksum: u64,
}

impl RecordHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4] = self.flags;
        // b[5..8] reserved, zero
        b[8..12].copy_from_slice(&self.raw_len.to_le_bytes());
        b[12..16].copy_from_slice(&self.stored_len.to_le_bytes());
        b[16..32].copy_from_slice(&self.hash);
        b[32..40].copy_from_slice(&self.checksum.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self, &'static str> {
        if b.len() < HEADER_LEN {
            return Err("short header");
        }
        if u32::from_le_bytes(b[0..4].try_into().unwrap()) != MAGIC {
            return Err("bad magic");
        }
        let flags = b[4];
        if flags & !FLAG_COMPRESSED != 0 || b[5..8] != [0, 0, 0] {
            return Err("unknown flags");
        }
        Ok(Self {
            flags,
            raw_len: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            stored_len: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            hash: b[16..32].try_into().unwrap(),
            checksum: u64::from_le_bytes(b[32..40].try_into().unwrap()),
        })
    }

    /// Total bytes the record occupies in a pack (header + payload).
    pub fn record_len(&self) -> u64 {
        HEADER_LEN as u64 + self.stored_len as u64
    }
}

/// A block ready to be appended to a pack.
#[derive(Debug, Clone)]
pub struct EncodedBlock {
    pub header: RecordHeader,
    pub payload: Vec<u8>,
}

thread_local! {
    static COMPRESSOR: RefCell<Option<(i32, zstd::bulk::Compressor<'static>)>> =
        const { RefCell::new(None) };
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> =
        const { RefCell::new(None) };
}

/// Compresses `data` (falling back to raw if zstd does not shrink it) and builds its header.
pub fn encode_block(data: &[u8], hash: Hash128, level: i32) -> io::Result<EncodedBlock> {
    let compressed = COMPRESSOR.with(|c| -> io::Result<Vec<u8>> {
        let mut c = c.borrow_mut();
        if c.as_ref().map(|(l, _)| *l) != Some(level) {
            *c = Some((level, zstd::bulk::Compressor::new(level)?));
        }
        c.as_mut().unwrap().1.compress(data)
    })?;
    let (flags, payload) = if compressed.len() < data.len() {
        (FLAG_COMPRESSED, compressed)
    } else {
        (0, data.to_vec())
    };
    Ok(EncodedBlock {
        header: RecordHeader {
            flags,
            raw_len: data.len() as u32,
            stored_len: payload.len() as u32,
            hash,
            checksum: checksum(&payload),
        },
        payload,
    })
}

/// Verifies `payload` against `header` and writes the uncompressed block into `out`.
/// `out.len()` must equal `header.raw_len`.
pub fn decode_payload(
    header: &RecordHeader,
    payload: &[u8],
    out: &mut [u8],
) -> Result<(), &'static str> {
    if payload.len() != header.stored_len as usize {
        return Err("payload length mismatch");
    }
    if out.len() != header.raw_len as usize {
        return Err("output length mismatch");
    }
    if checksum(payload) != header.checksum {
        return Err("checksum mismatch");
    }
    if header.flags & FLAG_COMPRESSED != 0 {
        let n = DECOMPRESSOR
            .with(|d| -> io::Result<usize> {
                let mut d = d.borrow_mut();
                if d.is_none() {
                    *d = Some(zstd::bulk::Decompressor::new()?);
                }
                d.as_mut().unwrap().decompress_to_buffer(payload, out)
            })
            .map_err(|_| "decompression failed")?;
        if n != out.len() {
            return Err("decompressed length mismatch");
        }
    } else {
        out.copy_from_slice(payload);
    }
    Ok(())
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --lib codec`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/codec.rs src/lib.rs
git commit -m "feat: block codec with BLAKE3-128, xxh3 and zstd"
```

---

### Task 3: Manifest segment encoding

**Files:**
- Create: `src/manifest.rs`
- Modify: `src/lib.rs` (full new content below)

Pure functions for the manifest layout: a file's block ids are split into segments of 4096 `u64`
ids, and segment 0 starts with a 16-byte header holding the file length. Id 0 means "not cached".

**Interfaces:**
- Consumes: nothing.
- Produces: `BLOCKS_PER_SEGMENT: u64 = 4096`, `FILE_HEADER_LEN: usize = 16`, `MISSING: u64 = 0`,
  `block_count(len: u64, block_size: u32) -> u64`, `segment_count(blocks: u64) -> u32` (at least 1),
  `slots_in_segment(blocks: u64, seg: u32) -> usize`, `block_len(len: u64, block_size: u32, idx: u64) -> u64`,
  `encode_segment(file_len: Option<u64>, ids: &[u64]) -> Vec<u8>` (pass `Some(len)` only for segment 0),
  `file_len(seg0: &[u8]) -> u64`, `slot(value: &[u8], seg: u32, i: usize) -> u64`,
  `decode_ids(value: &[u8], seg: u32) -> Vec<u64>`.

- [ ] **Step 1: Write the failing tests**

Create `src/manifest.rs` containing only its test module for now (the implementation goes above it in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment0_roundtrip() {
        let v = encode_segment(Some(12345), &[1, 0, 7]);
        assert_eq!(v.len(), FILE_HEADER_LEN + 24);
        assert_eq!(file_len(&v), 12345);
        assert_eq!(slot(&v, 0, 2), 7);
        assert_eq!(decode_ids(&v, 0), vec![1, 0, 7]);
    }

    #[test]
    fn later_segment_has_no_header() {
        let v = encode_segment(None, &[5, 6]);
        assert_eq!(v.len(), 16);
        assert_eq!(slot(&v, 1, 0), 5);
        assert_eq!(decode_ids(&v, 1), vec![5, 6]);
    }

    #[test]
    fn counts_and_lengths() {
        let bs = 65536;
        assert_eq!(block_count(0, bs), 0);
        assert_eq!(block_count(1, bs), 1);
        assert_eq!(block_count(65536, bs), 1);
        assert_eq!(block_count(65537, bs), 2);
        assert_eq!(segment_count(0), 1);
        assert_eq!(segment_count(4096), 1);
        assert_eq!(segment_count(4097), 2);
        assert_eq!(block_len(65537, bs, 0), 65536);
        assert_eq!(block_len(65537, bs, 1), 1);
        assert_eq!(block_len(65537, bs, 2), 0);
        assert_eq!(slots_in_segment(0, 0), 0);
        assert_eq!(slots_in_segment(5000, 0), 4096);
        assert_eq!(slots_in_segment(5000, 1), 904);
        assert_eq!(slots_in_segment(5000, 2), 0);
    }
}
```

Replace `src/lib.rs` with:

```rust
//! A deduplicating, compressing block store for caching virtual file system content.
//!
//! Files are identified by caller-chosen byte strings and stored as fixed-size blocks.
//! Blocks are deduplicated by content (BLAKE3-128), compressed with zstd, and appended
//! to a small number of large pack files. Metadata lives in a redb database.

mod codec;
mod config;
mod crash;
mod error;
mod manifest;

pub use config::{CompactOptions, StoreConfig};
pub use error::{Error, Result};
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --lib manifest`
Expected: FAIL, compile error: unresolved names `encode_segment`, `block_count`, etc.

- [ ] **Step 3: Write the implementation**

Insert this **above** the `#[cfg(test)]` module in `src/manifest.rs`:

```rust
//! File manifest encoding. A file's manifest is split into segments of
//! `BLOCKS_PER_SEGMENT` block ids. Segment 0 starts with a `FILE_HEADER_LEN` header.
//! A block id of 0 means "not cached".

/// Block ids per manifest segment (4096 blocks = 256 MiB of data at 64 KiB blocks).
pub const BLOCKS_PER_SEGMENT: u64 = 4096;
/// Bytes of file header at the start of segment 0: len u64, flags u32, reserved u32.
pub const FILE_HEADER_LEN: usize = 16;
/// Block id meaning "not cached".
pub const MISSING: u64 = 0;

pub fn block_count(len: u64, block_size: u32) -> u64 {
    len.div_ceil(block_size as u64)
}

/// Number of segments for a file with `blocks` blocks. Always at least 1 (segment 0 holds the header).
pub fn segment_count(blocks: u64) -> u32 {
    blocks.div_ceil(BLOCKS_PER_SEGMENT).max(1) as u32
}

/// Number of block slots segment `seg` holds in a file with `blocks` blocks.
pub fn slots_in_segment(blocks: u64, seg: u32) -> usize {
    let start = seg as u64 * BLOCKS_PER_SEGMENT;
    (blocks.min(start + BLOCKS_PER_SEGMENT).saturating_sub(start)) as usize
}

/// Length in bytes of block `idx` in a file of length `len`.
pub fn block_len(len: u64, block_size: u32, idx: u64) -> u64 {
    let start = idx * block_size as u64;
    len.saturating_sub(start).min(block_size as u64)
}

pub fn encode_segment(file_len: Option<u64>, ids: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FILE_HEADER_LEN + ids.len() * 8);
    if let Some(len) = file_len {
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&[0u8; 8]);
    }
    for id in ids {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

fn ids_bytes(value: &[u8], seg: u32) -> &[u8] {
    if seg == 0 {
        &value[FILE_HEADER_LEN..]
    } else {
        value
    }
}

/// File length stored in segment 0.
pub fn file_len(seg0: &[u8]) -> u64 {
    u64::from_le_bytes(seg0[0..8].try_into().unwrap())
}

/// Block id in slot `i` of a segment value.
pub fn slot(value: &[u8], seg: u32, i: usize) -> u64 {
    let b = ids_bytes(value, seg);
    u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap())
}

pub fn decode_ids(value: &[u8], seg: u32) -> Vec<u64> {
    let (chunks, _) = ids_bytes(value, seg).as_chunks::<8>();
    chunks.iter().map(|c| u64::from_le_bytes(*c)).collect()
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --lib manifest`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/manifest.rs
git commit -m "feat: manifest segment encoding"
```

---

### Task 4: Index: redb tables, block refcounting

**Files:**
- Create: `src/index.rs`
- Modify: `src/lib.rs` (full new content below)

The only module that touches redb. Five tables: `files` (manifest segments keyed by
`(file_id, segment)`), `blocks` (`block_id -> BlockLoc`, 40 bytes including the block's hash, so
freeing a block can delete its dedup entry without reading the pack), `dedup` (`hash -> block_id`),
`packs` (`pack_id -> PackInfo`) and `meta`. Reads go through `IndexRead`, a snapshot whose tables
open lazily, so a `stat` opens one table rather than four. Writes go through
`Index::update(durable, |tables| ...)`, which commits only if the closure returns `Ok`.

Invariant maintained here: a `dedup` row exists exactly when a `blocks` row with that hash exists.

**Interfaces:**
- Consumes: `codec::{HEADER_LEN, Hash128}`, `error::{Error, Result}`.
- Produces:
  - `pub struct BlockLoc { pack: u32, offset: u64, stored_len: u32, raw_len: u32, refcount: u32, hash: Hash128 }` with `encode`, `decode` and `record_len()`.
  - `pub enum PackState { Active, Sealed, Retired }` and `pub struct PackInfo { live_bytes: u64, state: PackState }`.
  - `pub struct SegmentRef<'a>`: derefs to `[u8]`, zero-copy.
  - `pub struct Index` with `open(path: &Path, cache_bytes: usize) -> Result<Index>`, `read(&self) -> Result<IndexRead>`, `update<R>(&self, durable: bool, f: impl FnOnce(&mut Tables<'_>) -> Result<R>) -> Result<R>` and `size(&self) -> Result<(u64 /*page bytes*/, u64 /*stored bytes*/)>`.
  - `IndexRead`: `segment(file_id, seg) -> Result<Option<SegmentRef>>`, `block(id) -> Result<Option<BlockLoc>>`, `dedup(&hash) -> Result<Option<u64>>`, `packs() -> Result<Vec<(u32, PackInfo)>>`, and `for_each_segment`, `for_each_block`, `for_each_dedup`.
  - `Tables<'t>`: `segment` (owned `Vec<u8>`), `put_segment`, `remove_segment`, `block`, `put_block`, `dedup`, `meta`, `put_meta`, `pack`, `put_pack`, `remove_pack`, `packs`, `add_live(pack, delta: i64)`, `insert_block(loc) -> Result<u64>` (refcount 0, allocates the id, adds the dedup row and live bytes), `incref(id)`, `decref(id)` (frees at zero; a no-op for a missing id), and `remove_block(id, &loc)`.
  - The `META_*` key constants and `SCHEMA_VERSION = 1`.

- [ ] **Step 1: Write the failing tests**

Create `src/index.rs` containing only its test module for now (the implementation goes above it in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("index.redb"), 1 << 20).unwrap();
        (dir, index)
    }

    fn loc(pack: u32, hash: u8) -> BlockLoc {
        BlockLoc {
            pack,
            offset: 0,
            stored_len: 60,
            raw_len: 100,
            refcount: 0,
            hash: [hash; 16],
        }
    }

    #[test]
    fn block_loc_roundtrip() {
        let l = BlockLoc {
            pack: 3,
            offset: 1 << 33,
            stored_len: 5,
            raw_len: 6,
            refcount: 7,
            hash: [1; 16],
        };
        assert_eq!(BlockLoc::decode(&l.encode()), l);
        assert_eq!(l.record_len(), 45);
    }

    #[test]
    fn refcounting_frees_rows_and_live_bytes() {
        let (_d, index) = open();
        let id = index
            .update(false, |t| {
                t.put_pack(
                    1,
                    &PackInfo {
                        live_bytes: 0,
                        state: PackState::Active,
                    },
                )?;
                let id = t.insert_block(loc(1, 9))?;
                t.incref(id)?;
                t.incref(id)?;
                Ok(id)
            })
            .unwrap();
        assert_eq!(id, 1);
        let r = index.read().unwrap();
        assert_eq!(r.dedup(&[9; 16]).unwrap(), Some(1));
        assert_eq!(r.block(1).unwrap().unwrap().refcount, 2);
        assert_eq!(r.packs().unwrap()[0].1.live_bytes, 100);
        drop(r);

        index.update(false, |t| t.decref(id)).unwrap();
        assert_eq!(index.read().unwrap().block(1).unwrap().unwrap().refcount, 1);

        index.update(false, |t| t.decref(id)).unwrap();
        let r = index.read().unwrap();
        assert!(r.block(1).unwrap().is_none());
        assert!(r.dedup(&[9; 16]).unwrap().is_none());
        assert_eq!(r.packs().unwrap()[0].1.live_bytes, 0);
        drop(r);

        // decref of a missing block is a no-op
        index.update(false, |t| t.decref(id)).unwrap();
    }

    #[test]
    fn segments_are_zero_copy_readable() {
        let (_d, index) = open();
        index
            .update(false, |t| t.put_segment(b"file", 0, &[1, 2, 3]))
            .unwrap();
        let r = index.read().unwrap();
        assert_eq!(&*r.segment(b"file", 0).unwrap().unwrap(), &[1, 2, 3]);
        assert!(r.segment(b"file", 1).unwrap().is_none());
        assert!(r.segment(b"fil", 0).unwrap().is_none());
    }

    #[test]
    fn failed_update_is_not_committed() {
        let (_d, index) = open();
        let res: Result<()> = index.update(false, |t| {
            t.put_meta("x", 1)?;
            Err(Error::NotFound)
        });
        assert!(res.is_err());
        assert_eq!(index.update(false, |t| t.meta("x")).unwrap(), None);
    }
}
```

Replace `src/lib.rs` with:

```rust
//! A deduplicating, compressing block store for caching virtual file system content.
//!
//! Files are identified by caller-chosen byte strings and stored as fixed-size blocks.
//! Blocks are deduplicated by content (BLAKE3-128), compressed with zstd, and appended
//! to a small number of large pack files. Metadata lives in a redb database.

mod codec;
mod config;
mod crash;
mod error;
mod index;
mod manifest;

pub use config::{CompactOptions, StoreConfig};
pub use error::{Error, Result};
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --lib index`
Expected: FAIL, compile error: unresolved names `Index`, `BlockLoc`, `PackInfo`.

- [ ] **Step 3: Write the implementation**

Insert this **above** the `#[cfg(test)]` module in `src/index.rs`:

```rust
//! Metadata index on redb. This is the only module that touches redb; nothing outside it sees redb types.

use std::cell::OnceCell;
use std::ops::Deref;
use std::path::Path;

use redb::{
    AccessGuard, Database, Durability, Key, ReadOnlyTable, ReadTransaction, ReadableDatabase,
    ReadableTable, Table, TableDefinition, Value,
};

use crate::codec::{HEADER_LEN, Hash128};
use crate::error::{Error, Result};

/// Manifest segments, keyed by (file id, segment number).
type FileKey = (&'static [u8], u32);

const FILES: TableDefinition<FileKey, &[u8]> = TableDefinition::new("files");
const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");
const DEDUP: TableDefinition<Hash128, u64> = TableDefinition::new("dedup");
const PACKS: TableDefinition<u32, &[u8]> = TableDefinition::new("packs");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

pub const META_SCHEMA_VERSION: &str = "schema_version";
pub const META_BLOCK_SIZE: &str = "block_size";
pub const META_NEXT_BLOCK_ID: &str = "next_block_id";
pub const META_NEXT_PACK_ID: &str = "next_pack_id";
pub const META_CLEAN_SHUTDOWN: &str = "clean_shutdown";
pub const SCHEMA_VERSION: u64 = 1;

/// Location and refcount of one stored block (row of the `blocks` table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockLoc {
    pub pack: u32,
    pub offset: u64,
    pub stored_len: u32,
    pub raw_len: u32,
    pub refcount: u32,
    pub hash: Hash128,
}

impl BlockLoc {
    pub const ENCODED_LEN: usize = 40;

    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut b = [0u8; Self::ENCODED_LEN];
        b[0..4].copy_from_slice(&self.pack.to_le_bytes());
        b[4..12].copy_from_slice(&self.offset.to_le_bytes());
        b[12..16].copy_from_slice(&self.stored_len.to_le_bytes());
        b[16..20].copy_from_slice(&self.raw_len.to_le_bytes());
        b[20..24].copy_from_slice(&self.refcount.to_le_bytes());
        b[24..40].copy_from_slice(&self.hash);
        b
    }

    pub fn decode(b: &[u8]) -> Self {
        Self {
            pack: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            offset: u64::from_le_bytes(b[4..12].try_into().unwrap()),
            stored_len: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            raw_len: u32::from_le_bytes(b[16..20].try_into().unwrap()),
            refcount: u32::from_le_bytes(b[20..24].try_into().unwrap()),
            hash: b[24..40].try_into().unwrap(),
        }
    }

    /// Bytes the record occupies in its pack.
    pub fn record_len(&self) -> u64 {
        HEADER_LEN as u64 + self.stored_len as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackState {
    Active = 0,
    Sealed = 1,
    Retired = 2,
}

/// Row of the `packs` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackInfo {
    pub live_bytes: u64,
    pub state: PackState,
}

impl PackInfo {
    pub fn encode(&self) -> [u8; 9] {
        let mut b = [0u8; 9];
        b[0..8].copy_from_slice(&self.live_bytes.to_le_bytes());
        b[8] = self.state as u8;
        b
    }

    pub fn decode(b: &[u8]) -> Self {
        let state = match b[8] {
            0 => PackState::Active,
            1 => PackState::Sealed,
            _ => PackState::Retired,
        };
        Self {
            live_bytes: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            state,
        }
    }
}

/// Zero-copy view of a manifest segment value, valid while its `IndexRead` lives.
pub struct SegmentRef<'a>(AccessGuard<'a, &'static [u8]>);

impl Deref for SegmentRef<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.0.value()
    }
}

pub struct Index {
    db: Database,
}

impl Index {
    pub fn open(path: &Path, cache_bytes: usize) -> Result<Self> {
        let db = Database::builder()
            .set_cache_size(cache_bytes)
            .create(path)?;
        let index = Self { db };
        // Create all tables so read transactions can always open them.
        index.update(true, |_| Ok(()))?;
        Ok(index)
    }

    /// (bytes of pages in use, bytes of keys and values stored). Briefly takes the write lock.
    pub fn size(&self) -> Result<(u64, u64)> {
        let txn = self.db.begin_write()?;
        let s = txn.stats()?;
        let pages = s.allocated_pages() * s.page_size() as u64;
        let stored = s.stored_bytes();
        txn.abort()?;
        Ok((pages, stored))
    }

    /// Opens a snapshot for reading.
    pub fn read(&self) -> Result<IndexRead> {
        Ok(IndexRead {
            txn: self.db.begin_read()?,
            files: OnceCell::new(),
            blocks: OnceCell::new(),
            dedup: OnceCell::new(),
            packs: OnceCell::new(),
        })
    }

    /// Runs `f` in one write transaction and commits it if `f` returns `Ok`.
    /// `durable = false` commits with `Durability::None`. redb allows one write transaction at a
    /// time, so this blocks while another thread is inside `update`.
    pub fn update<R>(
        &self,
        durable: bool,
        f: impl FnOnce(&mut Tables<'_>) -> Result<R>,
    ) -> Result<R> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(if durable {
            Durability::Immediate
        } else {
            Durability::None
        })?;
        let result = {
            let mut tables = Tables {
                files: txn.open_table(FILES)?,
                blocks: txn.open_table(BLOCKS)?,
                dedup: txn.open_table(DEDUP)?,
                packs: txn.open_table(PACKS)?,
                meta: txn.open_table(META)?,
            };
            f(&mut tables)?
        };
        txn.commit()?;
        Ok(result)
    }
}

/// A read snapshot of the index. Tables are opened on first use, so a snapshot that only
/// touches one table (such as `stat`) pays for one.
pub struct IndexRead {
    txn: ReadTransaction,
    files: OnceCell<ReadOnlyTable<FileKey, &'static [u8]>>,
    blocks: OnceCell<ReadOnlyTable<u64, &'static [u8]>>,
    dedup: OnceCell<ReadOnlyTable<Hash128, u64>>,
    packs: OnceCell<ReadOnlyTable<u32, &'static [u8]>>,
}

fn lazy_table<'a, K: Key + 'static, V: Value + 'static>(
    txn: &ReadTransaction,
    cell: &'a OnceCell<ReadOnlyTable<K, V>>,
    def: TableDefinition<K, V>,
) -> Result<&'a ReadOnlyTable<K, V>> {
    if let Some(t) = cell.get() {
        return Ok(t);
    }
    let t = txn.open_table(def)?;
    Ok(cell.get_or_init(|| t))
}

impl IndexRead {
    pub fn segment(&self, file_id: &[u8], seg: u32) -> Result<Option<SegmentRef<'_>>> {
        Ok(lazy_table(&self.txn, &self.files, FILES)?
            .get((file_id, seg))?
            .map(SegmentRef))
    }

    pub fn block(&self, id: u64) -> Result<Option<BlockLoc>> {
        Ok(lazy_table(&self.txn, &self.blocks, BLOCKS)?
            .get(id)?
            .map(|g| BlockLoc::decode(g.value())))
    }

    pub fn dedup(&self, hash: &Hash128) -> Result<Option<u64>> {
        Ok(lazy_table(&self.txn, &self.dedup, DEDUP)?
            .get(hash)?
            .map(|g| g.value()))
    }

    pub fn packs(&self) -> Result<Vec<(u32, PackInfo)>> {
        let mut out = Vec::new();
        for e in lazy_table(&self.txn, &self.packs, PACKS)?.iter()? {
            let (k, v) = e?;
            out.push((k.value(), PackInfo::decode(v.value())));
        }
        Ok(out)
    }

    pub fn for_each_segment(
        &self,
        mut f: impl FnMut(&[u8], u32, &[u8]) -> Result<()>,
    ) -> Result<()> {
        for e in lazy_table(&self.txn, &self.files, FILES)?.iter()? {
            let (k, v) = e?;
            let (id, seg) = k.value();
            f(id, seg, v.value())?;
        }
        Ok(())
    }

    pub fn for_each_block(&self, mut f: impl FnMut(u64, BlockLoc) -> Result<()>) -> Result<()> {
        for e in lazy_table(&self.txn, &self.blocks, BLOCKS)?.iter()? {
            let (k, v) = e?;
            f(k.value(), BlockLoc::decode(v.value()))?;
        }
        Ok(())
    }

    pub fn for_each_dedup(&self, mut f: impl FnMut(Hash128, u64) -> Result<()>) -> Result<()> {
        for e in lazy_table(&self.txn, &self.dedup, DEDUP)?.iter()? {
            let (k, v) = e?;
            f(k.value(), v.value())?;
        }
        Ok(())
    }
}

/// The tables of one write transaction.
pub struct Tables<'t> {
    files: Table<'t, FileKey, &'static [u8]>,
    blocks: Table<'t, u64, &'static [u8]>,
    dedup: Table<'t, Hash128, u64>,
    packs: Table<'t, u32, &'static [u8]>,
    meta: Table<'t, &'static str, u64>,
}

impl Tables<'_> {
    pub fn segment(&self, file_id: &[u8], seg: u32) -> Result<Option<Vec<u8>>> {
        Ok(self.files.get((file_id, seg))?.map(|g| g.value().to_vec()))
    }

    pub fn put_segment(&mut self, file_id: &[u8], seg: u32, value: &[u8]) -> Result<()> {
        self.files.insert((file_id, seg), value)?;
        Ok(())
    }

    pub fn remove_segment(&mut self, file_id: &[u8], seg: u32) -> Result<()> {
        self.files.remove((file_id, seg))?;
        Ok(())
    }

    pub fn block(&self, id: u64) -> Result<Option<BlockLoc>> {
        Ok(self.blocks.get(id)?.map(|g| BlockLoc::decode(g.value())))
    }

    pub fn put_block(&mut self, id: u64, loc: &BlockLoc) -> Result<()> {
        self.blocks.insert(id, loc.encode().as_slice())?;
        Ok(())
    }

    pub fn dedup(&self, hash: &Hash128) -> Result<Option<u64>> {
        Ok(self.dedup.get(hash)?.map(|g| g.value()))
    }

    pub fn meta(&self, key: &str) -> Result<Option<u64>> {
        Ok(self.meta.get(key)?.map(|g| g.value()))
    }

    pub fn put_meta(&mut self, key: &str, value: u64) -> Result<()> {
        self.meta.insert(key, value)?;
        Ok(())
    }

    pub fn pack(&self, id: u32) -> Result<Option<PackInfo>> {
        Ok(self.packs.get(id)?.map(|g| PackInfo::decode(g.value())))
    }

    pub fn put_pack(&mut self, id: u32, info: &PackInfo) -> Result<()> {
        self.packs.insert(id, info.encode().as_slice())?;
        Ok(())
    }

    pub fn remove_pack(&mut self, id: u32) -> Result<()> {
        self.packs.remove(id)?;
        Ok(())
    }

    /// Adds `delta` (may be negative) to a pack's live byte count.
    pub fn add_live(&mut self, pack: u32, delta: i64) -> Result<()> {
        let mut info = self
            .pack(pack)?
            .ok_or_else(|| Error::Corrupt(format!("block references unknown pack {pack}")))?;
        info.live_bytes = info.live_bytes.checked_add_signed(delta).ok_or_else(|| {
            Error::Corrupt(format!("live byte accounting underflow in pack {pack}"))
        })?;
        self.put_pack(pack, &info)
    }

    /// Inserts a new block with refcount 0 and its dedup entry. Returns the new block id.
    pub fn insert_block(&mut self, loc: BlockLoc) -> Result<u64> {
        let id = self.meta(META_NEXT_BLOCK_ID)?.unwrap_or(1);
        self.put_meta(META_NEXT_BLOCK_ID, id + 1)?;
        let loc = BlockLoc { refcount: 0, ..loc };
        self.put_block(id, &loc)?;
        self.dedup.insert(&loc.hash, id)?;
        self.add_live(loc.pack, loc.record_len() as i64)?;
        Ok(id)
    }

    pub fn incref(&mut self, id: u64) -> Result<()> {
        let mut loc = self
            .block(id)?
            .ok_or_else(|| Error::Corrupt(format!("incref of unknown block {id}")))?;
        loc.refcount += 1;
        self.put_block(id, &loc)
    }

    /// Drops one reference. At zero the block's rows are removed and its pack's live bytes reduced.
    /// Decrementing a block that no longer exists (a healed block) is a no-op.
    pub fn decref(&mut self, id: u64) -> Result<()> {
        let Some(mut loc) = self.block(id)? else {
            return Ok(());
        };
        if loc.refcount > 1 {
            loc.refcount -= 1;
            return self.put_block(id, &loc);
        }
        self.remove_block(id, &loc)
    }

    /// Removes a block's `blocks` and `dedup` rows and its live bytes, regardless of refcount.
    pub fn remove_block(&mut self, id: u64, loc: &BlockLoc) -> Result<()> {
        self.blocks.remove(id)?;
        if self.dedup(&loc.hash)? == Some(id) {
            self.dedup.remove(&loc.hash)?;
        }
        self.add_live(loc.pack, -(loc.record_len() as i64))
    }

    pub fn packs(&self) -> Result<Vec<(u32, PackInfo)>> {
        let mut out = Vec::new();
        for e in self.packs.iter()? {
            let (k, v) = e?;
            out.push((k.value(), PackInfo::decode(v.value())));
        }
        Ok(out)
    }
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --lib index`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/index.rs src/lib.rs
git commit -m "feat: redb index with refcounted blocks"
```

---

### Task 5: Manifest edits inside a transaction

**Files:**
- Create: `src/files.rs`
- Modify: `src/lib.rs` (full new content below)

Operations on a file's manifest within one index write transaction: replace a run of slots,
resize (creating the file if needed), and remove. `resize` implements the spec's `set_len` rule:
blocks past the new end are dropped, and so is the last block shared by the old and new lengths if
its length changes, whether the file shrinks mid-block or grows past a short last block. The
functions return the dropped or replaced ids; callers own the refcount changes.

**Interfaces:**
- Consumes: `index::Tables`, `manifest::*`, `error::{Error, Result}`.
- Produces:
  - `len(t: &Tables, file_id) -> Result<Option<u64>>`.
  - `set_slots(t: &mut Tables, file_id, len: u64, first: u64, ids: &[u64]) -> Result<Vec<u64>>`: returns the old ids.
  - `resize(t: &mut Tables, file_id, new_len: u64, block_size: u32) -> Result<Vec<u64>>`: returns the dropped, non-missing ids.
  - `remove(t: &mut Tables, file_id, block_size: u32) -> Result<Option<Vec<u64>>>`: `None` if the file does not exist.

- [ ] **Step 1: Write the failing tests**

Create `src/files.rs` containing only its test module for now (the implementation goes above it in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;

    const BS: u32 = 4096;
    const SEG_BYTES: u64 = BLOCKS_PER_SEGMENT * BS as u64;

    fn open() -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("i.redb"), 1 << 20).unwrap();
        (dir, index)
    }

    fn all_slots(index: &Index, id: &[u8]) -> Vec<u64> {
        index
            .update(false, |t| {
                let len = len(t, id)?.unwrap();
                let mut out = Vec::new();
                for s in 0..segment_count(block_count(len, BS)) {
                    out.extend(decode_ids(&t.segment(id, s)?.unwrap(), s));
                }
                Ok(out)
            })
            .unwrap()
    }

    #[test]
    fn create_set_and_shrink() {
        let (_d, index) = open();
        index
            .update(false, |t| resize(t, b"f", 3 * BS as u64, BS).map(|_| ()))
            .unwrap();
        assert_eq!(all_slots(&index, b"f"), vec![0, 0, 0]);
        let old = index
            .update(false, |t| set_slots(t, b"f", 3 * BS as u64, 1, &[7, 8]))
            .unwrap();
        assert_eq!(old, vec![0, 0]);
        assert_eq!(all_slots(&index, b"f"), vec![0, 7, 8]);
        // Cut through block 1: blocks 1 (length changes) and 2 (past end) are dropped.
        let dropped = index
            .update(false, |t| resize(t, b"f", BS as u64 + 10, BS))
            .unwrap();
        assert_eq!(dropped, vec![7, 8]);
        assert_eq!(all_slots(&index, b"f"), vec![0, 0]);
    }

    #[test]
    fn growing_drops_a_short_last_block() {
        let (_d, index) = open();
        index
            .update(false, |t| resize(t, b"f", 10, BS).map(|_| ()))
            .unwrap();
        index
            .update(false, |t| set_slots(t, b"f", 10, 0, &[5]).map(|_| ()))
            .unwrap();
        let dropped = index
            .update(false, |t| resize(t, b"f", 2 * BS as u64, BS))
            .unwrap();
        assert_eq!(dropped, vec![5]);
        assert_eq!(all_slots(&index, b"f"), vec![0, 0]);
        // Growing a file whose last block is full keeps it.
        index
            .update(false, |t| {
                set_slots(t, b"f", 2 * BS as u64, 0, &[1, 2]).map(|_| ())
            })
            .unwrap();
        let dropped = index
            .update(false, |t| resize(t, b"f", 3 * BS as u64, BS))
            .unwrap();
        assert!(dropped.is_empty());
        assert_eq!(all_slots(&index, b"f"), vec![1, 2, 0]);
    }

    #[test]
    fn multi_segment_resize_and_remove() {
        let (_d, index) = open();
        let size = 2 * SEG_BYTES + 5;
        index
            .update(false, |t| resize(t, b"big", size, BS).map(|_| ()))
            .unwrap();
        let n = block_count(size, BS) as usize;
        let ids: Vec<u64> = (1..=n as u64).collect();
        index
            .update(false, |t| set_slots(t, b"big", size, 0, &ids).map(|_| ()))
            .unwrap();
        assert_eq!(all_slots(&index, b"big"), ids);

        // Shrink into the second segment; segment 0 header must reflect the new length.
        let new_len = SEG_BYTES + 3 * BS as u64;
        let dropped = index
            .update(false, |t| resize(t, b"big", new_len, BS))
            .unwrap();
        assert_eq!(dropped, ids[4099..].to_vec());
        assert_eq!(all_slots(&index, b"big"), ids[..4099].to_vec());
        index
            .update(false, |t| {
                assert_eq!(len(t, b"big")?, Some(new_len));
                assert!(t.segment(b"big", 2)?.is_none());
                Ok(())
            })
            .unwrap();

        let removed = index
            .update(false, |t| remove(t, b"big", BS))
            .unwrap()
            .unwrap();
        assert_eq!(removed, ids[..4099].to_vec());
        assert!(
            index
                .update(false, |t| remove(t, b"big", BS))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn empty_file() {
        let (_d, index) = open();
        index
            .update(false, |t| resize(t, b"e", 0, BS).map(|_| ()))
            .unwrap();
        assert_eq!(all_slots(&index, b"e"), Vec::<u64>::new());
        index
            .update(false, |t| resize(t, b"e", 1, BS).map(|_| ()))
            .unwrap();
        assert_eq!(all_slots(&index, b"e"), vec![0]);
    }
}
```

Replace `src/lib.rs` with:

```rust
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
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --lib files`
Expected: FAIL, compile error: unresolved names `resize`, `set_slots`, `remove`, `len`.

- [ ] **Step 3: Write the implementation**

Insert this **above** the `#[cfg(test)]` module in `src/files.rs`:

```rust
//! Manifest edits inside an index write transaction.

use crate::error::{Error, Result};
use crate::index::Tables;
use crate::manifest::{
    BLOCKS_PER_SEGMENT, MISSING, block_count, block_len, decode_ids, encode_segment, file_len,
    segment_count, slots_in_segment,
};

fn missing_segment(file_id: &[u8], seg: u32) -> Error {
    Error::Corrupt(format!(
        "manifest segment {seg} missing for file {:?}",
        String::from_utf8_lossy(file_id)
    ))
}

/// File length, or `None` if the file does not exist.
pub fn len(t: &Tables<'_>, file_id: &[u8]) -> Result<Option<u64>> {
    Ok(t.segment(file_id, 0)?.map(|v| file_len(&v)))
}

/// Replaces slots `[first, first + ids.len())` and returns the ids they held before.
pub fn set_slots(
    t: &mut Tables<'_>,
    file_id: &[u8],
    len: u64,
    first: u64,
    ids: &[u64],
) -> Result<Vec<u64>> {
    let mut old = Vec::with_capacity(ids.len());
    let mut block = first;
    let mut done = 0;
    while done < ids.len() {
        let seg = (block / BLOCKS_PER_SEGMENT) as u32;
        let off = (block % BLOCKS_PER_SEGMENT) as usize;
        let value = t
            .segment(file_id, seg)?
            .ok_or_else(|| missing_segment(file_id, seg))?;
        let mut slots = decode_ids(&value, seg);
        let take = (slots.len() - off).min(ids.len() - done);
        for k in 0..take {
            old.push(std::mem::replace(&mut slots[off + k], ids[done + k]));
        }
        t.put_segment(
            file_id,
            seg,
            &encode_segment((seg == 0).then_some(len), &slots),
        )?;
        done += take;
        block += take as u64;
    }
    Ok(old)
}

/// Creates the file (all blocks missing) or changes its length.
/// Returns the non-missing ids of blocks that were dropped: blocks past the new end, and a
/// block whose length changes (the old or new last block).
pub fn resize(
    t: &mut Tables<'_>,
    file_id: &[u8],
    new_len: u64,
    block_size: u32,
) -> Result<Vec<u64>> {
    let nb = block_count(new_len, block_size);
    let new_segs = segment_count(nb);
    let Some(seg0) = t.segment(file_id, 0)? else {
        for s in 0..new_segs {
            let slots = vec![MISSING; slots_in_segment(nb, s)];
            t.put_segment(
                file_id,
                s,
                &encode_segment((s == 0).then_some(new_len), &slots),
            )?;
        }
        return Ok(Vec::new());
    };
    let old_len = file_len(&seg0);
    let ob = block_count(old_len, block_size);
    let old_segs = segment_count(ob);
    let common = ob.min(nb);
    let first_seg = (common.saturating_sub(1) / BLOCKS_PER_SEGMENT) as u32;
    let base = first_seg as u64 * BLOCKS_PER_SEGMENT;

    // Slots for blocks [base, ob)
    let mut slots = Vec::new();
    for s in first_seg..old_segs {
        let value = if s == 0 {
            seg0.clone()
        } else {
            t.segment(file_id, s)?
                .ok_or_else(|| missing_segment(file_id, s))?
        };
        slots.extend(decode_ids(&value, s));
    }

    let mut dropped = Vec::new();
    if common > 0 {
        let last = common - 1;
        if block_len(old_len, block_size, last) != block_len(new_len, block_size, last) {
            dropped.push(std::mem::replace(
                &mut slots[(last - base) as usize],
                MISSING,
            ));
        }
    }
    let keep = (nb - base) as usize;
    if slots.len() > keep {
        dropped.extend(slots.drain(keep..));
    } else {
        slots.resize(keep, MISSING);
    }
    dropped.retain(|&id| id != MISSING);

    for s in first_seg..new_segs {
        let lo = (s as u64 * BLOCKS_PER_SEGMENT - base) as usize;
        let hi = (((s as u64 + 1) * BLOCKS_PER_SEGMENT).min(nb) - base) as usize;
        t.put_segment(
            file_id,
            s,
            &encode_segment((s == 0).then_some(new_len), &slots[lo..hi]),
        )?;
    }
    if first_seg > 0 {
        // Segment 0 was not rewritten above but its header holds the length.
        t.put_segment(
            file_id,
            0,
            &encode_segment(Some(new_len), &decode_ids(&seg0, 0)),
        )?;
    }
    for s in new_segs..old_segs {
        t.remove_segment(file_id, s)?;
    }
    Ok(dropped)
}

/// Removes every segment of a file. Returns its non-missing block ids, or `None` if it does not exist.
pub fn remove(t: &mut Tables<'_>, file_id: &[u8], block_size: u32) -> Result<Option<Vec<u64>>> {
    let Some(seg0) = t.segment(file_id, 0)? else {
        return Ok(None);
    };
    let segs = segment_count(block_count(file_len(&seg0), block_size));
    let mut ids = Vec::new();
    for s in 0..segs {
        let value = if s == 0 {
            seg0.clone()
        } else {
            t.segment(file_id, s)?
                .ok_or_else(|| missing_segment(file_id, s))?
        };
        ids.extend(
            decode_ids(&value, s)
                .into_iter()
                .filter(|&id| id != MISSING),
        );
        t.remove_segment(file_id, s)?;
    }
    Ok(Some(ids))
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --lib files`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/files.rs src/lib.rs
git commit -m "feat: manifest edits for set_len, writes and deletes"
```

---

### Task 6: Pack files: writer and positioned readers

**Files:**
- Create: `src/pack.rs`
- Modify: `src/lib.rs` (full new content below)

Append-only pack files named `00000001.pack`. `PackWriter` owns the single active pack behind a
1 MiB `BufWriter` and fsyncs the old pack when it starts a new one. `PackFiles` caches read-only
handles and does positioned reads (`read_exact_at` on Unix, a `seek_read` loop on Windows), so many
threads can read the same handle concurrently. `close(id)` drops a cached handle so the file can
be deleted on Windows.

**Interfaces:**
- Consumes: std only.
- Produces:
  - `pack_path(dir, id) -> PathBuf`, `parse_pack_name(&str) -> Option<u32>`, `list_pack_ids(dir) -> io::Result<Vec<u32>>` (sorted), `remove_pack_file(dir, id) -> io::Result<()>` (a missing file counts as success).
  - `PackFiles::new(dir)`, `get(id) -> io::Result<Arc<File>>`, `read_exact_at(id, &mut buf, offset) -> io::Result<()>` (a short read gives `UnexpectedEof`), `close(id)`.
  - `PackWriter::new(dir, max_pack_size)`, `resume(id)`, `active_id() -> Option<u32>`, `needs_new_pack(record_len) -> bool` (an empty pack always accepts one record), `start_pack(id) -> io::Result<Option<u32>>` (returns the previous id after syncing it), `append(header: &[u8], payload: &[u8]) -> io::Result<(u32, u64)>`, `flush_buffer()` (makes bytes visible to readers, no fsync) and `sync()` (flush plus `sync_data`).

- [ ] **Step 1: Write the failing tests**

Create `src/pack.rs` containing only its test module for now (the implementation goes above it in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pack_names() {
        assert_eq!(parse_pack_name("00000012.pack"), Some(12));
        assert_eq!(parse_pack_name("12.pack"), None);
        assert_eq!(parse_pack_name("0000001x.pack"), None);
        assert_eq!(parse_pack_name("00000012.tmp"), None);
        assert_eq!(
            pack_path(Path::new("p"), 7),
            Path::new("p").join("00000007.pack")
        );
    }

    #[test]
    fn append_rotate_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = PackWriter::new(dir.path().to_path_buf(), 100);
        assert!(w.needs_new_pack(10));
        assert_eq!(w.start_pack(1).unwrap(), None);
        assert!(!w.needs_new_pack(200)); // empty pack accepts anything
        assert_eq!(w.append(b"head", b"payload").unwrap(), (1, 0));
        assert_eq!(w.append(b"h2", b"p2").unwrap(), (1, 11));
        assert!(w.needs_new_pack(90));
        w.flush_buffer().unwrap();

        let files = PackFiles::new(dir.path().to_path_buf());
        let mut buf = [0u8; 7];
        files.read_exact_at(1, &mut buf, 4).unwrap();
        assert_eq!(&buf, b"payload");
        assert!(files.read_exact_at(1, &mut [0u8; 10], 10).is_err());

        assert_eq!(w.start_pack(2).unwrap(), Some(1));
        assert_eq!(w.active_id(), Some(2));
        assert_eq!(list_pack_ids(dir.path()).unwrap(), vec![1, 2]);
    }

    #[test]
    fn resume_continues_at_end() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = PackWriter::new(dir.path().to_path_buf(), 1000);
        w.start_pack(5).unwrap();
        w.append(b"abc", b"def").unwrap();
        w.sync().unwrap();
        drop(w);
        let mut w = PackWriter::new(dir.path().to_path_buf(), 1000);
        w.resume(5).unwrap();
        assert_eq!(w.append(b"x", b"y").unwrap(), (5, 6));
    }

    #[test]
    fn closed_pack_can_be_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = PackWriter::new(dir.path().to_path_buf(), 1000);
        w.start_pack(1).unwrap();
        w.append(b"a", b"b").unwrap();
        w.start_pack(2).unwrap();
        let files = PackFiles::new(dir.path().to_path_buf());
        files.read_exact_at(1, &mut [0u8; 2], 0).unwrap();
        files.close(1);
        std::fs::remove_file(pack_path(dir.path(), 1)).unwrap();
    }
}
```

Replace `src/lib.rs` with:

```rust
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
mod pack;

pub use config::{CompactOptions, StoreConfig};
pub use error::{Error, Result};
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --lib pack`
Expected: FAIL, compile error: unresolved names `PackWriter`, `PackFiles`, `parse_pack_name`.

- [ ] **Step 3: Write the implementation**

Insert this **above** the `#[cfg(test)]` module in `src/pack.rs`:

```rust
//! Pack files: append-only files of block records.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub fn pack_path(dir: &Path, id: u32) -> PathBuf {
    dir.join(format!("{id:08}.pack"))
}

/// Parses a pack file name such as `00000012.pack` into its id.
pub fn parse_pack_name(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".pack")?;
    if stem.len() != 8 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse().ok()
}

/// Deletes a pack file. A file that is already gone counts as deleted.
pub fn remove_pack_file(dir: &Path, id: u32) -> io::Result<()> {
    match std::fs::remove_file(pack_path(dir, id)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Lists pack ids present in `dir`.
pub fn list_pack_ids(dir: &Path) -> io::Result<Vec<u32>> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        if let Some(id) = entry?.file_name().to_str().and_then(parse_pack_name) {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

#[cfg(unix)]
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, offset) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Shared read handles to pack files, opened on first use.
pub struct PackFiles {
    dir: PathBuf,
    handles: RwLock<HashMap<u32, Arc<File>>>,
}

impl PackFiles {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            handles: RwLock::new(HashMap::new()),
        }
    }

    pub fn get(&self, id: u32) -> io::Result<Arc<File>> {
        if let Some(f) = self.handles.read().unwrap().get(&id) {
            return Ok(f.clone());
        }
        let file = Arc::new(File::open(pack_path(&self.dir, id))?);
        Ok(self
            .handles
            .write()
            .unwrap()
            .entry(id)
            .or_insert(file)
            .clone())
    }

    /// Positioned read; safe to call from many threads at once.
    pub fn read_exact_at(&self, id: u32, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let file = self.get(id)?;
        read_exact_at(&file, buf, offset)
    }

    /// Drops the cached handle so the file can be deleted (required on Windows).
    pub fn close(&self, id: u32) {
        self.handles.write().unwrap().remove(&id);
    }
}

struct ActivePack {
    id: u32,
    out: BufWriter<File>,
    len: u64,
}

/// Appends records to the single active pack.
pub struct PackWriter {
    dir: PathBuf,
    max_pack_size: u64,
    active: Option<ActivePack>,
}

impl PackWriter {
    pub fn new(dir: PathBuf, max_pack_size: u64) -> Self {
        Self {
            dir,
            max_pack_size,
            active: None,
        }
    }

    /// Continues appending to an existing pack after a clean shutdown.
    pub fn resume(&mut self, id: u32) -> io::Result<()> {
        let file = OpenOptions::new()
            .append(true)
            .open(pack_path(&self.dir, id))?;
        let len = file.metadata()?.len();
        self.active = Some(ActivePack {
            id,
            out: BufWriter::with_capacity(1 << 20, file),
            len,
        });
        Ok(())
    }

    pub fn active_id(&self) -> Option<u32> {
        self.active.as_ref().map(|a| a.id)
    }

    /// True if a record of `record_len` bytes needs a new pack first.
    /// An empty pack always accepts one record, however large.
    pub fn needs_new_pack(&self, record_len: u64) -> bool {
        match &self.active {
            None => true,
            Some(a) => a.len > 0 && a.len + record_len > self.max_pack_size,
        }
    }

    /// Syncs and closes the current pack (if any) and creates pack `id`. Returns the previous pack id.
    pub fn start_pack(&mut self, id: u32) -> io::Result<Option<u32>> {
        let old = match self.active.take() {
            Some(mut a) => {
                a.out.flush()?;
                a.out.get_ref().sync_data()?;
                Some(a.id)
            }
            None => None,
        };
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(pack_path(&self.dir, id))?;
        self.active = Some(ActivePack {
            id,
            out: BufWriter::with_capacity(1 << 20, file),
            len: 0,
        });
        Ok(old)
    }

    /// Appends one record (header + payload). Returns (pack id, offset).
    pub fn append(&mut self, header: &[u8], payload: &[u8]) -> io::Result<(u32, u64)> {
        let a = self.active.as_mut().expect("append without an active pack");
        let offset = a.len;
        a.out.write_all(header)?;
        a.out.write_all(payload)?;
        a.len += (header.len() + payload.len()) as u64;
        Ok((a.id, offset))
    }

    /// Hands buffered bytes to the OS so readers can see them (no fsync).
    pub fn flush_buffer(&mut self) -> io::Result<()> {
        if let Some(a) = &mut self.active {
            a.out.flush()?;
        }
        Ok(())
    }

    /// Flushes and fsyncs the active pack.
    pub fn sync(&mut self) -> io::Result<()> {
        if let Some(a) = &mut self.active {
            a.out.flush()?;
            a.out.get_ref().sync_data()?;
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --lib pack`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/pack.rs
git commit -m "feat: pack file writer and positioned readers"
```

---

### Task 7: Read generation tracker

**Files:**
- Create: `src/tracker.rs`
- Modify: `src/lib.rs` (full new content below)

Lets compaction delete a retired pack only after every read that might still see it has
finished. A read calls `enter()` before opening its index snapshot and holds the guard until it is
done. After the commit that retires a pack, compaction calls `advance()` and keeps the returned
generation. The file may be deleted once `is_clear_before(generation)` is true.

**Interfaces:**
- Consumes: std only.
- Produces: `ReadTracker` (`Default`) with `enter(&self) -> ReadGuard<'_>`, `advance(&self) -> u64` and `is_clear_before(&self, generation: u64) -> bool`.

- [ ] **Step 1: Write the failing tests**

Create `src/tracker.rs` containing only its test module for now (the implementation goes above it in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_older_reads_only() {
        let t = ReadTracker::default();
        let old = t.enter();
        let g = t.advance();
        assert!(!t.is_clear_before(g));
        let new = t.enter();
        drop(old);
        assert!(t.is_clear_before(g));
        let g2 = t.advance();
        assert!(!t.is_clear_before(g2));
        drop(new);
        assert!(t.is_clear_before(g2));
    }
}
```

Replace `src/lib.rs` with:

```rust
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
mod pack;
mod tracker;

pub use config::{CompactOptions, StoreConfig};
pub use error::{Error, Result};
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --lib tracker`
Expected: FAIL, compile error: unresolved name `ReadTracker`.

- [ ] **Step 3: Write the implementation**

Insert this **above** the `#[cfg(test)]` module in `src/tracker.rs`:

```rust
//! Read generation tracking, so retired packs are deleted only after every read that
//! might still reference them has finished.

use std::collections::BTreeMap;
use std::sync::Mutex;

#[derive(Default)]
pub struct ReadTracker {
    state: Mutex<TrackerState>,
}

#[derive(Default)]
struct TrackerState {
    generation: u64,
    /// generation -> number of reads in progress that entered during it
    active: BTreeMap<u64, usize>,
}

pub struct ReadGuard<'a> {
    tracker: &'a ReadTracker,
    generation: u64,
}

impl ReadTracker {
    /// Call before opening the index read transaction.
    pub fn enter(&self) -> ReadGuard<'_> {
        let mut s = self.state.lock().unwrap();
        let generation = s.generation;
        *s.active.entry(generation).or_insert(0) += 1;
        ReadGuard {
            tracker: self,
            generation,
        }
    }

    /// Starts a new generation and returns it. Call after the commit that retires a pack:
    /// reads entering from now on cannot see the retired pack.
    pub fn advance(&self) -> u64 {
        let mut s = self.state.lock().unwrap();
        s.generation += 1;
        s.generation
    }

    /// True once no read that entered before generation `generation` is still running.
    pub fn is_clear_before(&self, generation: u64) -> bool {
        let s = self.state.lock().unwrap();
        s.active
            .keys()
            .next()
            .is_none_or(|&oldest| oldest >= generation)
    }
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        let mut s = self.tracker.state.lock().unwrap();
        let n = s.active.get_mut(&self.generation).unwrap();
        *n -= 1;
        if *n == 0 {
            s.active.remove(&self.generation);
        }
    }
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --lib tracker`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/tracker.rs
git commit -m "feat: read generation tracker for safe pack deletion"
```

---

### Task 8: BlockStore lifecycle: open, recovery, flush, metadata operations, stats

**Files:**
- Create: `tests/common/mod.rs`
- Create: `tests/lifecycle.rs`
- Create: `src/stats.rs`
- Create: `src/store.rs`
- Modify: `src/lib.rs` (full new content below)

The `BlockStore` struct and everything that doesn't move block data:

- **`open`:** validates the config, takes the exclusive `LOCK` with `File::try_lock` (returning `Locked` if another process holds it), opens the index, then runs `recover` in one durable transaction, which does the following:
  1. Checks `block_size` and the schema version.
  2. Reads and clears the clean-shutdown flag.
  3. Deletes retired packs and any pack files not registered in the index.
  4. Resumes the newest active pack only after a clean shutdown. After a crash it seals that pack instead, so a torn tail is never appended to.
- **`flush` and `close`:** both go through `durable_commit`, which holds the writer lock while it fsyncs the active pack and makes an `Immediate` commit. Nothing can be appended between the sync and the commit, so a durable index never points at unsynced bytes. `close`, and `Drop`, also set the clean-shutdown flag.
- **`append_records`** (used by later tasks): appends to the active pack and starts new packs as needed. A new pack is registered in the index *before* its file is created, so a crash leaves at most an orphan row, which `recover` cleans up.
- **Metadata:** `stat`, `set_len`, `delete`, `cached_ranges` and `stats`.

**Interfaces:**
- Consumes: `config::StoreConfig`, `error::*`, `index::*`, `manifest::*`, `pack::*`, `tracker::ReadTracker`, `files`, `crash`.
- Produces (public): `BlockStore::open(dir: impl AsRef<Path>, cfg: StoreConfig) -> Result<BlockStore>`, `close(self) -> Result<()>`, `flush(&self) -> Result<()>`, `stat(&self, file_id: &[u8]) -> Result<Option<FileInfo>>`, `set_len(&self, file_id, len: u64) -> Result<()>`, `delete(&self, file_id) -> Result<()>`, `cached_ranges(&self, file_id) -> Result<Vec<Range<u64>>>`, `stats(&self) -> Result<Stats>` and `index_size(&self) -> Result<IndexSize>`. `pub struct FileInfo { len: u64 }`, `pub struct ReadResult { bytes: usize, missing: Vec<Range<u64>> }` (filled in by Task 10), `pub struct Stats { packs: Vec<PackStats>, healed_blocks: u64, unflushed_bytes: u64 }`, `pub struct PackStats { id, sealed, file_bytes, live_bytes }` with `garbage_ratio()`, and `pub struct IndexSize { page_bytes, stored_bytes }`.
- Produces (crate-internal, used by Tasks 9-12): the fields `cfg`, `pack_dir`, `index`, `packs`, `writer: Mutex<Writer>`, `tracker`, `retired: Mutex<Vec<(u32, u64)>>`, `compact_lock`, `pool`, `unflushed` and `healed`; the methods `durable_commit(f)`, `maybe_auto_flush()`, `append_records(iter of (header, payload)) -> Result<Vec<(u32, u64)>>`, `check_id(file_id)` and `install(f)` (runs `f` on the configured rayon pool); and `push_range(&mut Vec<Range<u64>>, Range<u64>)`.
- Test helpers in `tests/common/mod.rs`: `BS = 4096`, `test_config()` (4 KiB blocks, 64 KiB packs, 32 KiB write transactions), `open(dir)`, `random_bytes(seed, len)`, `pattern_bytes(seed, len)` and `read_all(store, id)`. `read_all` needs `read` from Task 10; nothing calls it before then.

> **Note:** Expect `dead_code` warnings for crate-internal items (`append_records`, `install`, `tracker`, `retired`, `push_range`, ...) until Tasks 9-12 use them. `read_all` in `tests/common/mod.rs` calls `store.read`, which does not exist until Task 10. So in this task, create `tests/common/mod.rs` **without** the `read_all` function, and add it in Task 10 Step 1.

- [ ] **Step 1: Write the failing tests**

Create `tests/common/mod.rs`:

```rust
#![allow(dead_code)]

use block_store::{BlockStore, StoreConfig};

pub const BS: usize = 4096;

/// Small blocks and packs so tests exercise rotation and segments quickly.
pub fn test_config() -> StoreConfig {
    StoreConfig {
        block_size: BS as u32,
        max_pack_size: 64 * 1024,
        index_cache_bytes: 4 << 20,
        write_txn_bytes: 8 * BS,
        ..StoreConfig::default()
    }
}

pub fn open(dir: &std::path::Path) -> BlockStore {
    BlockStore::open(dir, test_config()).unwrap()
}

/// Deterministic incompressible bytes.
pub fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    blake3::Hasher::new()
        .update(&seed.to_le_bytes())
        .finalize_xof()
        .fill(&mut out);
    out
}

/// Compressible bytes: a repeated seed-dependent pattern.
pub fn pattern_bytes(seed: u64, len: usize) -> Vec<u8> {
    (0..len).map(|i| (seed as usize + i / 64) as u8).collect()
}
```

Create `tests/lifecycle.rs`:

```rust
mod common;

use block_store::{BlockStore, Error, StoreConfig};
use common::*;

#[test]
fn set_len_creates_and_stat_reports_length() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    assert!(store.stat(b"f").unwrap().is_none());
    store.set_len(b"f", 10 * BS as u64 + 7).unwrap();
    assert_eq!(store.stat(b"f").unwrap().unwrap().len, 10 * BS as u64 + 7);
    assert!(store.cached_ranges(b"f").unwrap().is_empty());
    store.set_len(b"f", 3).unwrap();
    assert_eq!(store.stat(b"f").unwrap().unwrap().len, 3);
}

#[test]
fn delete_removes_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.set_len(b"f", 100).unwrap();
    store.delete(b"f").unwrap();
    assert!(store.stat(b"f").unwrap().is_none());
    assert!(matches!(store.delete(b"f"), Err(Error::NotFound)));
    assert!(matches!(store.cached_ranges(b"f"), Err(Error::NotFound)));
}

#[test]
fn file_id_length_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    assert!(matches!(store.stat(&[0u8; 257]), Err(Error::FileIdTooLong)));
    assert!(matches!(
        store.set_len(&[0u8; 257], 1),
        Err(Error::FileIdTooLong)
    ));
    store.set_len(&[7u8; 256], 1).unwrap();
}

#[test]
fn metadata_survives_close_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = open(dir.path());
        store.set_len(b"f", 12345).unwrap();
        store.close().unwrap();
    }
    let store = open(dir.path());
    assert_eq!(store.stat(b"f").unwrap().unwrap().len, 12345);
}

#[test]
fn flush_makes_writes_durable_without_close() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.set_len(b"f", 99).unwrap();
    store.flush().unwrap();
    assert_eq!(store.stats().unwrap().unflushed_bytes, 0);
    drop(store);
    assert_eq!(open(dir.path()).stat(b"f").unwrap().unwrap().len, 99);
}

#[test]
fn second_open_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let _store = open(dir.path());
    assert!(matches!(
        BlockStore::open(dir.path(), test_config()),
        Err(Error::Locked)
    ));
}

#[test]
fn block_size_is_fixed_at_creation() {
    let dir = tempfile::tempdir().unwrap();
    open(dir.path()).close().unwrap();
    let cfg = StoreConfig {
        block_size: 8192,
        ..test_config()
    };
    assert!(matches!(
        BlockStore::open(dir.path(), cfg),
        Err(Error::Config(_))
    ));
}

#[test]
fn invalid_config_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    for cfg in [
        StoreConfig {
            block_size: 1000,
            ..test_config()
        },
        StoreConfig {
            zstd_level: 99,
            ..test_config()
        },
        StoreConfig {
            max_pack_size: BS as u64,
            ..test_config()
        },
        StoreConfig {
            max_file_id_len: 0,
            ..test_config()
        },
    ] {
        assert!(matches!(
            BlockStore::open(dir.path(), cfg),
            Err(Error::Config(_))
        ));
    }
}

#[test]
fn store_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BlockStore>();
}
```

Create `src/stats.rs` containing only its test module for now (the implementation goes above it in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_ratio() {
        let p = PackStats {
            id: 1,
            sealed: true,
            file_bytes: 100,
            live_bytes: 25,
        };
        assert_eq!(p.garbage_ratio(), 0.75);
        let empty = PackStats {
            id: 2,
            sealed: true,
            file_bytes: 0,
            live_bytes: 0,
        };
        assert_eq!(empty.garbage_ratio(), 0.0);
    }
}
```

Replace `src/lib.rs` with:

```rust
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
mod pack;
mod stats;
mod store;
mod tracker;

pub use config::{CompactOptions, StoreConfig};
pub use error::{Error, Result};
pub use stats::{IndexSize, PackStats, Stats};
pub use store::{BlockStore, FileInfo, ReadResult};
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --lib stats && cargo test --test lifecycle`
Expected: FAIL, compile error: `unresolved import block_store::BlockStore` (and `PackStats` in the stats unit test).

- [ ] **Step 3: Write the implementation**

Insert this **above** the `#[cfg(test)]` module in `src/stats.rs`:

```rust
//! Space accounting.

use std::sync::atomic::Ordering;

use crate::error::Result;
use crate::index::PackState;
use crate::pack::pack_path;
use crate::store::BlockStore;

/// Space accounting for one pack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackStats {
    pub id: u32,
    /// False for the pack currently being appended to.
    pub sealed: bool,
    pub file_bytes: u64,
    pub live_bytes: u64,
}

impl PackStats {
    /// Fraction of the pack file that is garbage.
    pub fn garbage_ratio(&self) -> f64 {
        if self.file_bytes == 0 {
            return 0.0;
        }
        self.file_bytes.saturating_sub(self.live_bytes) as f64 / self.file_bytes as f64
    }
}

#[derive(Debug, Clone)]
pub struct Stats {
    /// Packs that are active or sealed (retired packs awaiting deletion are excluded).
    pub packs: Vec<PackStats>,
    /// Corrupt blocks dropped from the index since the store was opened.
    pub healed_blocks: u64,
    /// Bytes appended since the last durable flush.
    pub unflushed_bytes: u64,
}

impl BlockStore {
    pub fn stats(&self) -> Result<Stats> {
        let r = self.index.read()?;
        let mut packs = Vec::new();
        for (id, info) in r.packs()? {
            if info.state == PackState::Retired {
                continue;
            }
            let file_bytes = std::fs::metadata(pack_path(&self.pack_dir, id))
                .map(|m| m.len())
                .unwrap_or(0);
            packs.push(PackStats {
                id,
                sealed: info.state == PackState::Sealed,
                file_bytes,
                live_bytes: info.live_bytes,
            });
        }
        Ok(Stats {
            packs,
            healed_blocks: self.healed.load(Ordering::Relaxed),
            unflushed_bytes: self.unflushed.load(Ordering::Relaxed),
        })
    }

    /// Size of the metadata index. Briefly takes the index write lock.
    pub fn index_size(&self) -> Result<IndexSize> {
        let (page_bytes, stored_bytes) = self.index.size()?;
        Ok(IndexSize {
            page_bytes,
            stored_bytes,
        })
    }
}

/// Result of [`BlockStore::index_size`]. The index file itself grows in large steps and is
/// usually bigger than `page_bytes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexSize {
    /// Bytes of b-tree pages in use.
    pub page_bytes: u64,
    /// Bytes of keys and values, without b-tree overhead.
    pub stored_bytes: u64,
}
```

Create `src/store.rs`:

```rust
//! `BlockStore`: opening, recovery, flushing and file-level metadata operations.

use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::config::StoreConfig;
use crate::error::{Error, Result};
use crate::index::{
    Index, META_BLOCK_SIZE, META_CLEAN_SHUTDOWN, META_NEXT_PACK_ID, META_SCHEMA_VERSION, PackInfo,
    PackState, SCHEMA_VERSION, Tables,
};
use crate::manifest::{MISSING, block_count, decode_ids, file_len, segment_count};
use crate::pack::{PackFiles, PackWriter, list_pack_ids, remove_pack_file};
use crate::tracker::ReadTracker;
use crate::{crash, files};

/// Metadata of a stored file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileInfo {
    pub len: u64,
}

/// Result of [`BlockStore::read`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResult {
    /// Bytes covered by the read after clamping to end of file.
    pub bytes: usize,
    /// Byte ranges (whole blocks, clamped to end of file) that are not cached. The matching
    /// bytes of the caller's buffer are left unspecified.
    pub missing: Vec<Range<u64>>,
}

pub(crate) struct Writer {
    pub packs: PackWriter,
    pub next_pack_id: u32,
}

/// A deduplicating, compressing block store. Cheap to share between threads (`Send + Sync`).
pub struct BlockStore {
    pub(crate) cfg: StoreConfig,
    pub(crate) pack_dir: PathBuf,
    pub(crate) index: Index,
    pub(crate) packs: PackFiles,
    pub(crate) writer: Mutex<Writer>,
    pub(crate) tracker: ReadTracker,
    /// Retired packs waiting for older reads to finish: (pack id, generation).
    pub(crate) retired: Mutex<Vec<(u32, u64)>>,
    pub(crate) compact_lock: Mutex<()>,
    pub(crate) pool: Option<rayon::ThreadPool>,
    pub(crate) unflushed: AtomicU64,
    pub(crate) healed: AtomicU64,
    shut_down: AtomicBool,
    _lock: File,
}

impl BlockStore {
    /// Opens or creates a store in directory `dir`.
    pub fn open(dir: impl AsRef<Path>, cfg: StoreConfig) -> Result<Self> {
        cfg.validate()?;
        let dir = dir.as_ref();
        let pack_dir = dir.join("packs");
        std::fs::create_dir_all(&pack_dir)?;

        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("LOCK"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Error::Locked),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }

        let index = Index::open(&dir.join("index.redb"), cfg.index_cache_bytes)?;
        let (resume, next_pack_id) = index.update(true, |t| recover(t, &pack_dir, &cfg))?;

        let mut packs = PackWriter::new(pack_dir.clone(), cfg.max_pack_size);
        if let Some(id) = resume {
            packs.resume(id)?;
        }
        let pool = cfg
            .compression_threads
            .map(|n| rayon::ThreadPoolBuilder::new().num_threads(n).build())
            .transpose()
            .map_err(|e| Error::Config(e.to_string()))?;

        Ok(Self {
            packs: PackFiles::new(pack_dir.clone()),
            writer: Mutex::new(Writer {
                packs,
                next_pack_id,
            }),
            pack_dir,
            index,
            tracker: ReadTracker::default(),
            retired: Mutex::new(Vec::new()),
            compact_lock: Mutex::new(()),
            pool,
            unflushed: AtomicU64::new(0),
            healed: AtomicU64::new(0),
            shut_down: AtomicBool::new(false),
            _lock: lock,
            cfg,
        })
    }

    /// Flushes and marks the store cleanly closed. Dropping the store does the same, but ignores errors.
    pub fn close(self) -> Result<()> {
        self.shutdown()
    }

    fn shutdown(&self) -> Result<()> {
        if self.shut_down.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.durable_commit(|t| t.put_meta(META_CLEAN_SHUTDOWN, 1))
    }

    /// Makes every write that completed before this call durable.
    pub fn flush(&self) -> Result<()> {
        self.durable_commit(|_| Ok(()))
    }

    /// Syncs pack data, then commits `f` with `Durability::Immediate`. The writer lock is held
    /// throughout, so no record can be appended between the sync and the durable commit.
    pub(crate) fn durable_commit<R>(
        &self,
        f: impl FnOnce(&mut Tables<'_>) -> Result<R>,
    ) -> Result<R> {
        let mut w = self.writer.lock().unwrap();
        w.packs.sync()?;
        crash::point("flush_before_commit");
        let r = self.index.update(true, f)?;
        self.unflushed.store(0, Ordering::Relaxed);
        drop(w);
        Ok(r)
    }

    pub(crate) fn maybe_auto_flush(&self) -> Result<()> {
        if self.unflushed.load(Ordering::Relaxed) >= self.cfg.auto_flush_bytes {
            self.flush()?;
        }
        Ok(())
    }

    /// Appends (header, payload) records to the active pack, starting new packs as needed.
    /// Returns (pack id, offset) per record. Records are visible to readers when this returns.
    /// This is the only place the writer lock is held while calling `Index::update`; `update`
    /// callbacks never take the writer lock, so this cannot deadlock.
    pub(crate) fn append_records<'a>(
        &self,
        records: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
    ) -> Result<Vec<(u32, u64)>> {
        let mut w = self.writer.lock().unwrap();
        let mut out = Vec::new();
        let mut bytes = 0u64;
        for (header, payload) in records {
            let len = (header.len() + payload.len()) as u64;
            if w.packs.needs_new_pack(len) {
                let id = w.next_pack_id;
                w.next_pack_id += 1;
                let old = w.packs.active_id();
                // Register before creating the file: an orphan row is cleaned up on open, and no
                // commit can reference a pack before its registration commit.
                self.index.update(false, |t| {
                    if let Some(old) = old
                        && let Some(mut info) = t.pack(old)?
                    {
                        info.state = PackState::Sealed;
                        t.put_pack(old, &info)?;
                    }
                    t.put_pack(
                        id,
                        &PackInfo {
                            live_bytes: 0,
                            state: PackState::Active,
                        },
                    )?;
                    t.put_meta(META_NEXT_PACK_ID, id as u64 + 1)
                })?;
                w.packs.start_pack(id)?;
            }
            out.push(w.packs.append(header, payload)?);
            bytes += len;
        }
        w.packs.flush_buffer()?;
        self.unflushed.fetch_add(bytes, Ordering::Relaxed);
        Ok(out)
    }

    pub(crate) fn check_id(&self, file_id: &[u8]) -> Result<()> {
        if file_id.len() > self.cfg.max_file_id_len {
            return Err(Error::FileIdTooLong);
        }
        Ok(())
    }

    pub(crate) fn install<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R {
        match &self.pool {
            Some(pool) => pool.install(f),
            None => f(),
        }
    }

    /// File metadata, or `None` if the file does not exist. One index lookup.
    pub fn stat(&self, file_id: &[u8]) -> Result<Option<FileInfo>> {
        self.check_id(file_id)?;
        let r = self.index.read()?;
        Ok(r.segment(file_id, 0)?
            .map(|s| FileInfo { len: file_len(&s) }))
    }

    /// Creates the file if it does not exist (every block missing), or changes its length.
    /// Blocks past the new end, and a last block whose length changes, become missing.
    pub fn set_len(&self, file_id: &[u8], len: u64) -> Result<()> {
        self.check_id(file_id)?;
        let bs = self.cfg.block_size;
        self.index.update(false, |t| {
            for id in files::resize(t, file_id, len, bs)? {
                t.decref(id)?;
            }
            Ok(())
        })
    }

    /// Deletes a file. Its blocks are freed once no other file references them.
    pub fn delete(&self, file_id: &[u8]) -> Result<()> {
        self.check_id(file_id)?;
        let bs = self.cfg.block_size;
        self.index.update(false, |t| {
            for id in files::remove(t, file_id, bs)?.ok_or(Error::NotFound)? {
                t.decref(id)?;
            }
            Ok(())
        })
    }

    /// Cached byte ranges of a file, merged and in order.
    pub fn cached_ranges(&self, file_id: &[u8]) -> Result<Vec<Range<u64>>> {
        self.check_id(file_id)?;
        let bs = self.cfg.block_size as u64;
        let r = self.index.read()?;
        let len = file_len(&r.segment(file_id, 0)?.ok_or(Error::NotFound)?);
        let mut out: Vec<Range<u64>> = Vec::new();
        for s in 0..segment_count(block_count(len, bs as u32)) {
            let value = r
                .segment(file_id, s)?
                .ok_or_else(|| Error::Corrupt(format!("manifest segment {s} missing")))?;
            for (i, id) in decode_ids(&value, s).into_iter().enumerate() {
                if id == MISSING || r.block(id)?.is_none() {
                    continue;
                }
                let start = (s as u64 * crate::manifest::BLOCKS_PER_SEGMENT + i as u64) * bs;
                push_range(&mut out, start..(start + bs).min(len));
            }
        }
        Ok(out)
    }
}

impl Drop for BlockStore {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            tracing::warn!(error = %e, "block store shutdown failed");
        }
    }
}

/// Appends `r` to `out`, merging with the last range when they touch.
pub(crate) fn push_range(out: &mut Vec<Range<u64>>, r: Range<u64>) {
    if let Some(last) = out.last_mut()
        && last.end == r.start
    {
        last.end = r.end;
        return;
    }
    out.push(r);
}

/// Runs in the first transaction after opening. Validates metadata, reconciles pack files with the
/// `packs` table and marks the store as not cleanly shut down.
/// Returns (pack to resume appending to, next pack id).
fn recover(t: &mut Tables<'_>, pack_dir: &Path, cfg: &StoreConfig) -> Result<(Option<u32>, u32)> {
    match t.meta(META_BLOCK_SIZE)? {
        None => {
            t.put_meta(META_BLOCK_SIZE, cfg.block_size as u64)?;
            t.put_meta(META_SCHEMA_VERSION, SCHEMA_VERSION)?;
        }
        Some(bs) if bs != cfg.block_size as u64 => {
            return Err(Error::Config(format!(
                "store was created with block_size {bs}"
            )));
        }
        Some(_) => {}
    }
    if t.meta(META_SCHEMA_VERSION)? != Some(SCHEMA_VERSION) {
        return Err(Error::Config("unsupported index schema version".into()));
    }
    let clean = t.meta(META_CLEAN_SHUTDOWN)? == Some(1);
    t.put_meta(META_CLEAN_SHUTDOWN, 0)?;

    let on_disk: HashSet<u32> = list_pack_ids(pack_dir)?.into_iter().collect();
    let mut active = Vec::new();
    let mut registered = HashSet::new();
    for (id, info) in t.packs()? {
        registered.insert(id);
        if info.state == PackState::Retired {
            match remove_pack_file(pack_dir, id) {
                Ok(()) => t.remove_pack(id)?,
                // Stays retired; deletion is retried on the next open.
                Err(e) => tracing::warn!(pack = id, error = %e, "could not delete retired pack"),
            }
            continue;
        }
        if !on_disk.contains(&id) {
            if info.live_bytes == 0 {
                t.remove_pack(id)?;
                continue;
            }
            return Err(Error::Corrupt(format!("pack {id} is missing")));
        }
        if info.state == PackState::Active {
            active.push(id);
        }
    }
    // Pack files created after the last durable commit hold no referenced data.
    for &id in &on_disk {
        if !registered.contains(&id) {
            remove_pack_file(pack_dir, id)?;
        }
    }
    // Resume the newest active pack only after a clean shutdown; otherwise its tail may be torn.
    active.sort_unstable();
    let resume = if clean { active.pop() } else { None };
    for id in active {
        let mut info = t.pack(id)?.unwrap();
        info.state = PackState::Sealed;
        t.put_pack(id, &info)?;
    }
    let max_registered = registered.iter().copied().max().unwrap_or(0);
    let next = (t.meta(META_NEXT_PACK_ID)?.unwrap_or(1) as u32).max(max_registered + 1);
    Ok((resume, next))
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --lib stats && cargo test --test lifecycle`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/stats.rs src/store.rs tests/common/mod.rs tests/lifecycle.rs
git commit -m "feat: BlockStore open/recover/flush and metadata operations"
```

---

### Task 9: Write path: validation, dedup, parallel compression, commit

**Files:**
- Create: `tests/write.rs`
- Create: `src/write.rs`
- Modify: `src/lib.rs` (full new content below)

`write_blocks` validates alignment against the file length and splits large writes into
transactions of `write_txn_bytes`. A failure after the first chunk is reported as
`PartialWrite { blocks_written }`. `write_chunk` does the following:

1. Hashes the blocks in parallel.
2. Finds which hashes are not stored yet, using a read snapshot.
3. Compresses only those, in parallel with no locks held, deduplicating within the chunk.
4. Appends them with `append_records`.
5. Commits one transaction, which re-validates against the current file length, re-checks every dedup entry, inserts new blocks, sets the slots, increments the new ids and **then** decrements the old ones (so rewriting identical content keeps the block).

If a dedup hit from step 2 was freed by another commit in the meantime, the transaction is left
unmodified and the loop compresses and appends those blocks, then retries.

**Interfaces:**
- Consumes: `BlockStore::{stat, check_id, install, append_records, maybe_auto_flush}`, `Index::{read, update}`, `Tables::{dedup, insert_block, incref, decref}`, `files::{len, set_slots}`, `codec::{hash128, encode_block}`.
- Produces: `BlockStore::write_blocks(&self, file_id: &[u8], first_block: u64, data: &[u8]) -> Result<()>` and `pub(crate) fn validate_write(len: u64, block_size: u32, first: u64, data_len: usize) -> Result<()>`.

- [ ] **Step 1: Write the failing tests**

Create `tests/write.rs`:

```rust
mod common;

use block_store::{BlockStore, Error};
use common::*;

const RECORD: u64 = 40 + BS as u64; // header + one incompressible block stored raw

fn live_bytes(store: &BlockStore) -> u64 {
    store
        .stats()
        .unwrap()
        .packs
        .iter()
        .map(|p| p.live_bytes)
        .sum()
}

#[test]
fn write_errors() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    assert!(matches!(
        store.write_blocks(b"nope", 0, &[0u8; BS]),
        Err(Error::NotFound)
    ));
    store.set_len(b"f", 2 * BS as u64 + 10).unwrap();
    assert!(matches!(
        store.write_blocks(b"f", 0, &[0u8; 10]),
        Err(Error::Unaligned)
    ));
    assert!(matches!(
        store.write_blocks(b"f", 2, &[0u8; BS]),
        Err(Error::Unaligned)
    ));
    assert!(matches!(
        store.write_blocks(b"f", 3, &[0u8; 1]),
        Err(Error::OutOfRange)
    ));
    store.write_blocks(b"f", 2, &[0u8; 10]).unwrap();
}

#[test]
fn sparse_writes_show_in_cached_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let len = 4 * BS as u64 + 10;
    store.set_len(b"s", len).unwrap();
    store.write_blocks(b"s", 1, &random_bytes(2, BS)).unwrap();
    store
        .write_blocks(b"s", 3, &random_bytes(3, BS + 10))
        .unwrap();
    let bs = BS as u64;
    assert_eq!(
        store.cached_ranges(b"s").unwrap(),
        vec![bs..2 * bs, 3 * bs..len]
    );
}

#[test]
fn dedup_within_and_across_files() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let block = random_bytes(9, BS);
    let data = [block.clone(), block.clone(), block].concat();
    for id in [b"x", b"y"] {
        store.set_len(id, data.len() as u64).unwrap();
        store.write_blocks(id, 0, &data).unwrap();
    }
    assert_eq!(live_bytes(&store), RECORD, "one stored copy");
}

#[test]
fn compressible_blocks_are_stored_compressed() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.set_len(b"c", 4 * BS as u64).unwrap();
    store
        .write_blocks(b"c", 0, &pattern_bytes(1, 4 * BS))
        .unwrap();
    assert!(
        live_bytes(&store) < BS as u64,
        "{} bytes",
        live_bytes(&store)
    );
}

#[test]
fn overwrite_frees_the_old_block() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.set_len(b"f", BS as u64).unwrap();
    store.write_blocks(b"f", 0, &random_bytes(10, BS)).unwrap();
    store.write_blocks(b"f", 0, &random_bytes(11, BS)).unwrap();
    assert_eq!(live_bytes(&store), RECORD);
    // Rewriting identical content keeps the block.
    store.write_blocks(b"f", 0, &random_bytes(11, BS)).unwrap();
    assert_eq!(live_bytes(&store), RECORD);
}

#[test]
fn delete_and_shrink_free_unshared_blocks_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let shared = random_bytes(12, BS);
    store.set_len(b"a", 3 * BS as u64).unwrap();
    store.set_len(b"b", BS as u64).unwrap();
    store
        .write_blocks(
            b"a",
            0,
            &[shared.clone(), random_bytes(13, BS), random_bytes(14, BS)].concat(),
        )
        .unwrap();
    store.write_blocks(b"b", 0, &shared).unwrap();
    assert_eq!(live_bytes(&store), 3 * RECORD);
    store.set_len(b"a", 2 * BS as u64).unwrap();
    assert_eq!(live_bytes(&store), 2 * RECORD);
    store.delete(b"a").unwrap();
    assert_eq!(live_bytes(&store), RECORD);
    assert_eq!(store.cached_ranges(b"b").unwrap(), vec![0..BS as u64]);
}

#[test]
fn large_write_spans_transactions_segments_and_packs() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    // 4100 blocks: crosses a manifest segment (4096 blocks) and many 64 KiB packs.
    let len = 4100 * BS as u64;
    store.set_len(b"big", len).unwrap();
    store
        .write_blocks(b"big", 0, &random_bytes(14, len as usize))
        .unwrap();
    assert_eq!(store.cached_ranges(b"big").unwrap(), vec![0..len]);
    let stats = store.stats().unwrap();
    assert!(stats.packs.len() > 100);
    assert_eq!(
        stats.packs.iter().filter(|p| !p.sealed).count(),
        1,
        "exactly one active pack"
    );
    assert_eq!(live_bytes(&store), 4100 * RECORD);
}

#[test]
fn concurrent_writers() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    std::thread::scope(|s| {
        for t in 0..4u64 {
            let store = &store;
            s.spawn(move || {
                for i in 0..20u64 {
                    let id = format!("t{t}-{i}");
                    // Seeds overlap between threads, so writers race on the same dedup entries.
                    let data = random_bytes(i, 3 * BS);
                    store.set_len(id.as_bytes(), data.len() as u64).unwrap();
                    store.write_blocks(id.as_bytes(), 0, &data).unwrap();
                }
            });
        }
    });
    for t in 0..4 {
        for i in 0..20 {
            assert_eq!(
                store.cached_ranges(format!("t{t}-{i}").as_bytes()).unwrap(),
                vec![0..3 * BS as u64]
            );
        }
    }
    // 20 seeds x 3 distinct blocks each, shared by all four threads.
    assert_eq!(
        live_bytes(&store),
        60 * RECORD,
        "each distinct block stored once"
    );
}
```

Create `src/write.rs` containing only its test module for now (the implementation goes above it in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation() {
        let bs = 4096;
        // 2.5 blocks
        let len = 2 * 4096 + 2048;
        assert!(validate_write(len, bs, 0, 4096).is_ok());
        assert!(validate_write(len, bs, 0, 2 * 4096 + 2048).is_ok());
        assert!(validate_write(len, bs, 2, 2048).is_ok());
        assert!(validate_write(len, bs, 1, 0).is_ok());
        assert!(matches!(
            validate_write(len, bs, 0, 100),
            Err(Error::Unaligned)
        ));
        assert!(matches!(
            validate_write(len, bs, 2, 4096),
            Err(Error::Unaligned)
        ));
        assert!(matches!(
            validate_write(len, bs, 2, 2047),
            Err(Error::Unaligned)
        ));
        assert!(matches!(
            validate_write(len, bs, 3, 1),
            Err(Error::OutOfRange)
        ));
        assert!(matches!(
            validate_write(len, bs, 2, 4096 + 2048),
            Err(Error::OutOfRange)
        ));
        assert!(matches!(
            validate_write(len, bs, u64::MAX, 1),
            Err(Error::OutOfRange)
        ));
    }
}
```

Replace `src/lib.rs` with:

```rust
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
mod pack;
mod stats;
mod store;
mod tracker;
mod write;

pub use config::{CompactOptions, StoreConfig};
pub use error::{Error, Result};
pub use stats::{IndexSize, PackStats, Stats};
pub use store::{BlockStore, FileInfo, ReadResult};
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --lib write && cargo test --test write`
Expected: FAIL, compile error: `no method named write_blocks found for struct BlockStore`.

- [ ] **Step 3: Write the implementation**

Insert this **above** the `#[cfg(test)]` module in `src/write.rs`:

```rust
//! Write path: validation, dedup, parallel compression, append, index commit.

use std::collections::{HashMap, HashSet};

use rayon::prelude::*;

use crate::codec::{EncodedBlock, HEADER_LEN, Hash128, encode_block, hash128};
use crate::error::{Error, Result};
use crate::index::BlockLoc;
use crate::manifest::{MISSING, block_count, block_len};
use crate::store::BlockStore;
use crate::{crash, files};

/// Checks that `data_len` bytes written at block `first` are block-aligned for a file of `len` bytes.
pub(crate) fn validate_write(len: u64, block_size: u32, first: u64, data_len: usize) -> Result<()> {
    if data_len == 0 {
        return Ok(());
    }
    let bs = block_size as u64;
    let count = (data_len as u64).div_ceil(bs);
    let end = first.checked_add(count).ok_or(Error::OutOfRange)?;
    if end > block_count(len, block_size) {
        return Err(Error::OutOfRange);
    }
    let tail = data_len as u64 - (count - 1) * bs;
    if tail != block_len(len, block_size, end - 1) {
        return Err(Error::Unaligned);
    }
    Ok(())
}

impl BlockStore {
    /// Writes whole blocks starting at block index `first_block`. Every block must be exactly
    /// `block_size` bytes, except the file's final block, which must be exactly its remaining length.
    /// The file must exist (see [`BlockStore::set_len`]). Large writes are committed in chunks of
    /// about `write_txn_bytes`; if a later chunk fails the error is [`Error::PartialWrite`].
    pub fn write_blocks(&self, file_id: &[u8], first_block: u64, data: &[u8]) -> Result<()> {
        self.check_id(file_id)?;
        let bs = self.cfg.block_size;
        let len = self.stat(file_id)?.ok_or(Error::NotFound)?.len;
        validate_write(len, bs, first_block, data.len())?;

        let chunk_bytes = (self.cfg.write_txn_bytes / bs as usize).max(1) * bs as usize;
        let mut done = 0u64;
        for chunk in data.chunks(chunk_bytes) {
            let result = self
                .write_chunk(file_id, first_block + done, chunk)
                .map(|()| done += chunk.len().div_ceil(bs as usize) as u64)
                .and_then(|()| self.maybe_auto_flush());
            if let Err(e) = result {
                return Err(if done == 0 {
                    e
                } else {
                    Error::PartialWrite {
                        blocks_written: done,
                        source: Box::new(e),
                    }
                });
            }
        }
        Ok(())
    }

    /// Writes one chunk in one index transaction.
    fn write_chunk(&self, file_id: &[u8], first: u64, chunk: &[u8]) -> Result<()> {
        let bs = self.cfg.block_size;
        let blocks: Vec<&[u8]> = chunk.chunks(bs as usize).collect();
        let hashes: Vec<Hash128> = self.install(|| blocks.par_iter().map(|b| hash128(b)).collect());

        // Blocks whose content is not stored yet.
        let mut need: Vec<usize> = {
            let r = self.index.read()?;
            let mut seen = HashSet::new();
            let mut need = Vec::new();
            for (i, h) in hashes.iter().enumerate() {
                if seen.insert(*h) && r.dedup(h)?.is_none() {
                    need.push(i);
                }
            }
            need
        };

        let mut new_records: HashMap<Hash128, BlockLoc> = HashMap::new();
        loop {
            self.encode_and_append(&blocks, &hashes, &need, &mut new_records)?;
            crash::point("write_after_append");
            let retry = self.index.update(false, |t| {
                let len = files::len(t, file_id)?.ok_or(Error::NotFound)?;
                validate_write(len, bs, first, chunk.len())?;

                // A dedup hit seen earlier may have been freed by another commit since.
                // Check before modifying anything; committing an untouched transaction is harmless.
                let mut missing = Vec::new();
                for (i, h) in hashes.iter().enumerate() {
                    if !new_records.contains_key(h) && t.dedup(h)?.is_none() {
                        missing.push(i);
                    }
                }
                if !missing.is_empty() {
                    return Ok(Some(missing));
                }

                let mut ids = Vec::with_capacity(hashes.len());
                for h in &hashes {
                    let id = match t.dedup(h)? {
                        Some(id) => id,
                        None => t.insert_block(new_records[h])?,
                    };
                    ids.push(id);
                }
                let old = files::set_slots(t, file_id, len, first, &ids)?;
                // Increment before decrementing so rewriting a slot with the same block keeps it.
                for &id in &ids {
                    t.incref(id)?;
                }
                for id in old {
                    if id != MISSING {
                        t.decref(id)?;
                    }
                }
                Ok(None)
            })?;
            match retry {
                None => return Ok(()),
                Some(missing) => need = missing,
            }
        }
    }

    /// Compresses the blocks at `need` (skipping hashes already in `out`) and appends them.
    fn encode_and_append(
        &self,
        blocks: &[&[u8]],
        hashes: &[Hash128],
        need: &[usize],
        out: &mut HashMap<Hash128, BlockLoc>,
    ) -> Result<()> {
        let mut seen = HashSet::new();
        let todo: Vec<usize> = need
            .iter()
            .copied()
            .filter(|&i| !out.contains_key(&hashes[i]) && seen.insert(hashes[i]))
            .collect();
        if todo.is_empty() {
            return Ok(());
        }
        let level = self.cfg.zstd_level;
        let encoded: Vec<EncodedBlock> = self.install(|| {
            todo.par_iter()
                .map(|&i| encode_block(blocks[i], hashes[i], level))
                .collect::<std::io::Result<_>>()
        })?;
        let headers: Vec<[u8; HEADER_LEN]> = encoded.iter().map(|e| e.header.encode()).collect();
        let locs = self.append_records(
            headers
                .iter()
                .zip(&encoded)
                .map(|(h, e)| (h.as_slice(), e.payload.as_slice())),
        )?;
        for (e, (pack, offset)) in encoded.iter().zip(locs) {
            out.insert(
                e.header.hash,
                BlockLoc {
                    pack,
                    offset,
                    stored_len: e.header.stored_len,
                    raw_len: e.header.raw_len,
                    refcount: 0,
                    hash: e.header.hash,
                },
            );
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --lib write && cargo test --test write`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/write.rs tests/write.rs
git commit -m "feat: deduplicating, compressing write path"
```

---

### Task 10: Read path with self-healing

**Files:**
- Create: `tests/read.rs`
- Create: `src/read.rs`
- Modify: `src/lib.rs` (full new content below)
- Modify: `tests/common/mod.rs` (add `read_all`)

`read` enters the read tracker, opens one snapshot, walks the covered blocks (switching manifest
segments at 4096-block boundaries), and does a positioned read of each record into a thread-local
buffer. It verifies magic, lengths, hash and checksum, and decompresses straight into the caller's
buffer when a whole block is wanted. Missing blocks are merged into `missing` ranges. A corrupt
record is **healed**: its `blocks` and `dedup` rows are removed (only if the row still points at
the same location), `healed_blocks` goes up, and the block reads as missing.

`read_record` and `heal` are `pub(crate)` because verification (Task 11) and compaction (Task 12)
reuse them.

**Interfaces:**
- Consumes: `BlockStore::{check_id, tracker, index, packs, healed}`, `store::push_range`, `codec::{RecordHeader, decode_payload, HEADER_LEN}`, `manifest::{slot, file_len, BLOCKS_PER_SEGMENT, MISSING}`.
- Produces: `BlockStore::read(&self, file_id: &[u8], offset: u64, buf: &mut [u8]) -> Result<ReadResult>`, `pub(crate) fn read_record<R>(&self, loc: &BlockLoc, f: impl FnOnce(&RecordHeader, &[u8]) -> Result<R, &'static str>) -> Result<Result<R, &'static str>>` and `pub(crate) fn heal(&self, id: u64, loc: &BlockLoc, reason: &'static str) -> Result<()>`.

- [ ] **Step 0: Add `read_all` to the test helpers**

Add this function at the end of `tests/common/mod.rs` (it was left out in Task 8):

```rust
/// Reads a whole file, asserting nothing is missing.
pub fn read_all(store: &BlockStore, id: &[u8]) -> Vec<u8> {
    let len = store.stat(id).unwrap().unwrap().len as usize;
    let mut buf = vec![0u8; len];
    let r = store.read(id, 0, &mut buf).unwrap();
    assert_eq!(r.bytes, len);
    assert!(
        r.missing.is_empty(),
        "unexpected missing ranges {:?}",
        r.missing
    );
    buf
}
```

- [ ] **Step 1: Write the failing tests**

Create `tests/read.rs`:

```rust
mod common;

use block_store::{Error, ReadResult};
use common::*;

#[test]
fn write_then_read_whole_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let data = random_bytes(1, 3 * BS + 100);
    store.set_len(b"a", data.len() as u64).unwrap();
    store.write_blocks(b"a", 0, &data).unwrap();
    assert_eq!(read_all(&store, b"a"), data);
}

#[test]
fn sparse_file_reports_missing_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let len = 4 * BS + 10;
    store.set_len(b"s", len as u64).unwrap();
    let block1 = random_bytes(2, BS);
    let tail = random_bytes(3, 10);
    store.write_blocks(b"s", 1, &block1).unwrap();
    store.write_blocks(b"s", 4, &tail).unwrap();

    let mut buf = vec![0u8; len];
    let r = store.read(b"s", 0, &mut buf).unwrap();
    let bs = BS as u64;
    assert_eq!(
        r,
        ReadResult {
            bytes: len,
            missing: vec![0..bs, 2 * bs..4 * bs]
        }
    );
    assert_eq!(&buf[BS..2 * BS], &block1[..]);
    assert_eq!(&buf[4 * BS..], &tail[..]);
}

#[test]
fn partial_and_clamped_reads() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let data = pattern_bytes(7, 2 * BS + 5);
    store.set_len(b"p", data.len() as u64).unwrap();
    store.write_blocks(b"p", 0, &data).unwrap();

    let mut buf = vec![0u8; 100];
    let r = store.read(b"p", BS as u64 - 50, &mut buf).unwrap();
    assert_eq!(r.bytes, 100);
    assert_eq!(&buf[..], &data[BS - 50..BS + 50]);

    let mut buf = vec![0u8; 1000];
    let r = store.read(b"p", 2 * BS as u64, &mut buf).unwrap();
    assert_eq!(r.bytes, 5);
    assert_eq!(&buf[..5], &data[2 * BS..]);

    assert_eq!(store.read(b"p", 10 * BS as u64, &mut buf).unwrap().bytes, 0);
    assert!(matches!(
        store.read(b"nope", 0, &mut buf),
        Err(Error::NotFound)
    ));
}

#[test]
fn reads_across_manifest_segments() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let data = pattern_bytes(8, 4100 * BS);
    store.set_len(b"big", data.len() as u64).unwrap();
    store.write_blocks(b"big", 0, &data).unwrap();
    let mut buf = vec![0u8; 3 * BS];
    let offset = 4095 * BS - 17;
    store.read(b"big", offset as u64, &mut buf).unwrap();
    assert_eq!(buf, data[offset..offset + 3 * BS]);
}

#[test]
fn data_survives_reopen_and_appending_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let data = pattern_bytes(15, 5 * BS);
    {
        let store = open(dir.path());
        store.set_len(b"f", data.len() as u64).unwrap();
        store.write_blocks(b"f", 0, &data).unwrap();
        store.close().unwrap();
    }
    let store = open(dir.path());
    assert_eq!(read_all(&store, b"f"), data);
    let packs_before = store.stats().unwrap().packs.len();
    let g = random_bytes(16, BS);
    store.set_len(b"g", BS as u64).unwrap();
    store.write_blocks(b"g", 0, &g).unwrap();
    assert_eq!(
        store.stats().unwrap().packs.len(),
        packs_before,
        "clean reopen resumes the active pack"
    );
    assert_eq!(read_all(&store, b"g"), g);
}

#[test]
fn corrupt_block_heals_to_missing() {
    let dir = tempfile::tempdir().unwrap();
    let data = random_bytes(17, 2 * BS);
    {
        let store = open(dir.path());
        store.set_len(b"f", data.len() as u64).unwrap();
        store.write_blocks(b"f", 0, &data).unwrap();
        store.close().unwrap();
    }
    // Flip the first payload byte of the first record.
    let pack = dir.path().join("packs").join("00000001.pack");
    let mut bytes = std::fs::read(&pack).unwrap();
    bytes[40] ^= 0xff;
    std::fs::write(&pack, bytes).unwrap();

    let store = open(dir.path());
    let mut buf = vec![0u8; data.len()];
    let r = store.read(b"f", 0, &mut buf).unwrap();
    assert_eq!(r.missing, vec![0..BS as u64]);
    assert_eq!(&buf[BS..], &data[BS..]);
    assert_eq!(store.stats().unwrap().healed_blocks, 1);
    // Still missing on the next read, and rewriting restores it.
    assert_eq!(
        store.read(b"f", 0, &mut buf).unwrap().missing,
        vec![0..BS as u64]
    );
    store.write_blocks(b"f", 0, &data[..BS]).unwrap();
    assert_eq!(read_all(&store, b"f"), data);
}

#[test]
fn concurrent_readers_and_writers() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    std::thread::scope(|s| {
        for t in 0..4u64 {
            let store = &store;
            s.spawn(move || {
                for i in 0..20u64 {
                    let id = format!("t{t}-{i}");
                    let data = random_bytes(t * 1000 + i, 3 * BS);
                    store.set_len(id.as_bytes(), data.len() as u64).unwrap();
                    store.write_blocks(id.as_bytes(), 0, &data).unwrap();
                    assert_eq!(read_all(store, id.as_bytes()), data);
                }
            });
        }
    });
}
```

Replace `src/lib.rs` with:

```rust
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
mod pack;
mod read;
mod stats;
mod store;
mod tracker;
mod write;

pub use config::{CompactOptions, StoreConfig};
pub use error::{Error, Result};
pub use stats::{IndexSize, PackStats, Stats};
pub use store::{BlockStore, FileInfo, ReadResult};
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test read`
Expected: FAIL, compile error: `no method named read found for struct BlockStore` (from `read_all` and the tests).

- [ ] **Step 3: Write the implementation**

Create `src/read.rs`:

```rust
//! Read path: manifest lookup, positioned pack reads, verification, self-healing.

use std::cell::RefCell;
use std::io;
use std::sync::atomic::Ordering;

use crate::codec::{HEADER_LEN, RecordHeader, decode_payload};
use crate::error::{Error, Result};
use crate::index::BlockLoc;
use crate::manifest::{BLOCKS_PER_SEGMENT, MISSING, file_len, slot};
use crate::store::{BlockStore, ReadResult, push_range};

thread_local! {
    static RECORD_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static BLOCK_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

impl BlockStore {
    /// Reads up to `buf.len()` bytes at `offset`, clamped to end of file. Cached blocks are
    /// decoded into `buf`; blocks that are not cached are reported in `missing`.
    pub fn read(&self, file_id: &[u8], offset: u64, buf: &mut [u8]) -> Result<ReadResult> {
        self.check_id(file_id)?;
        let _guard = self.tracker.enter();
        let r = self.index.read()?;
        let seg0 = r.segment(file_id, 0)?.ok_or(Error::NotFound)?;
        let len = file_len(&seg0);
        if offset >= len || buf.is_empty() {
            return Ok(ReadResult {
                bytes: 0,
                missing: Vec::new(),
            });
        }
        let end = len.min(offset + buf.len() as u64);
        let bs = self.cfg.block_size as u64;
        let mut missing = Vec::new();

        let mut seg_no = 0u32;
        let mut seg = seg0;
        for b in offset / bs..=(end - 1) / bs {
            let want_seg = (b / BLOCKS_PER_SEGMENT) as u32;
            if want_seg != seg_no {
                seg = r.segment(file_id, want_seg)?.ok_or_else(|| {
                    Error::Corrupt(format!("manifest segment {want_seg} missing"))
                })?;
                seg_no = want_seg;
            }
            let id = slot(&seg, seg_no, (b % BLOCKS_PER_SEGMENT) as usize);
            let block_start = b * bs;
            let block_end = len.min(block_start + bs);
            let from = offset.max(block_start);
            let to = end.min(block_end);
            let dst = &mut buf[(from - offset) as usize..(to - offset) as usize];
            let found = match (id != MISSING).then(|| r.block(id)).transpose()?.flatten() {
                Some(loc) => self.read_block_into(id, &loc, (from - block_start) as usize, dst)?,
                None => false,
            };
            if !found {
                push_range(&mut missing, block_start..block_end);
            }
        }
        Ok(ReadResult {
            bytes: (end - offset) as usize,
            missing,
        })
    }

    /// Decodes bytes `[skip, skip + dst.len())` of block `id` into `dst`.
    /// Returns false (after healing) if the record is corrupt.
    fn read_block_into(
        &self,
        id: u64,
        loc: &BlockLoc,
        skip: usize,
        dst: &mut [u8],
    ) -> Result<bool> {
        if skip + dst.len() > loc.raw_len as usize {
            return Err(Error::Corrupt(format!(
                "block {id} is shorter than its file slot"
            )));
        }
        match self.read_record(loc, |header, payload| {
            if skip == 0 && dst.len() == header.raw_len as usize {
                return decode_payload(header, payload, dst);
            }
            BLOCK_BUF.with(|bb| {
                let mut bb = bb.borrow_mut();
                bb.resize(header.raw_len as usize, 0);
                decode_payload(header, payload, &mut bb)?;
                dst.copy_from_slice(&bb[skip..skip + dst.len()]);
                Ok(())
            })
        })? {
            Ok(()) => Ok(true),
            Err(reason) => {
                self.heal(id, loc, reason)?;
                Ok(false)
            }
        }
    }

    /// Reads the record at `loc`, checks its header against `loc`, and passes it to `f`.
    /// The outer `Result` carries I/O errors; the inner one describes corruption.
    pub(crate) fn read_record<R>(
        &self,
        loc: &BlockLoc,
        f: impl FnOnce(&RecordHeader, &[u8]) -> std::result::Result<R, &'static str>,
    ) -> Result<std::result::Result<R, &'static str>> {
        RECORD_BUF.with(|rb| {
            let mut rb = rb.borrow_mut();
            rb.resize(loc.record_len() as usize, 0);
            match self.packs.read_exact_at(loc.pack, &mut rb, loc.offset) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(Err("record truncated"));
                }
                Err(e) => return Err(e.into()),
            }
            let header = match RecordHeader::decode(&rb[..HEADER_LEN]) {
                Ok(h) => h,
                Err(reason) => return Ok(Err(reason)),
            };
            if header.stored_len != loc.stored_len
                || header.raw_len != loc.raw_len
                || header.hash != loc.hash
            {
                return Ok(Err("record does not match index"));
            }
            Ok(f(&header, &rb[HEADER_LEN..]))
        })
    }

    /// Drops a corrupt block from the index so it reads as missing and can be rewritten.
    pub(crate) fn heal(&self, id: u64, loc: &BlockLoc, reason: &'static str) -> Result<()> {
        tracing::warn!(
            block = id,
            pack = loc.pack,
            offset = loc.offset,
            reason,
            "dropping corrupt block"
        );
        self.index.update(false, |t| {
            if let Some(cur) = t.block(id)?
                && cur.pack == loc.pack
                && cur.offset == loc.offset
            {
                t.remove_block(id, &cur)?;
            }
            Ok(())
        })?;
        self.healed.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test read`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/read.rs tests/common/mod.rs tests/read.rs
git commit -m "feat: read path with checksum verification and self-healing"
```

---

### Task 11: verify(): full consistency check

**Files:**
- Create: `tests/verify.rs`
- Create: `src/verify.rs`
- Modify: `src/lib.rs` (full new content below)

An fsck used by the property and crash tests. It checks the following:

- **Manifests:** every file has exactly the expected segments and slot counts.
- **Refcounts:** each block's refcount equals the number of manifest references to it.
- **Dedup:** every block has its dedup entry, and every dedup entry points at a block with the same hash.
- **Pack accounting:** each pack's `live_bytes` equals the sum of its blocks' records, and each registered pack file exists.
- **Records:** every record decodes and passes its checksum.

Ids referenced by manifests but missing from `blocks` are healed blocks, not errors.

**Interfaces:**
- Consumes: `IndexRead::{for_each_segment, for_each_block, for_each_dedup, dedup, block, packs}`, `BlockStore::read_record`, `codec::decode_payload`, `manifest::*`.
- Produces: `BlockStore::verify(&self) -> Result<VerifyReport>` and `pub struct VerifyReport { files: u64, blocks: u64, problems: Vec<String> }` with `is_ok()`.

- [ ] **Step 1: Write the failing tests**

Create `tests/verify.rs`:

```rust
mod common;

use common::*;

fn populated(dir: &std::path::Path) {
    let store = open(dir);
    let shared = random_bytes(1, BS);
    for (id, extra) in [(&b"a"[..], 2u64), (b"b", 3), (b"c", 4)] {
        store.set_len(id, 3 * BS as u64 + 5).unwrap();
        store
            .write_blocks(
                id,
                0,
                &[shared.clone(), random_bytes(extra, 2 * BS)].concat(),
            )
            .unwrap();
        store
            .write_blocks(id, 3, &random_bytes(extra + 10, 5))
            .unwrap();
    }
    store.write_blocks(b"b", 1, &random_bytes(99, BS)).unwrap();
    store.set_len(b"c", 2 * BS as u64).unwrap();
    store.delete(b"a").unwrap();
    store.close().unwrap();
}

#[test]
fn verify_passes_after_mixed_operations() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());
    let report = open(dir.path()).verify().unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    assert_eq!(report.files, 2);
    // Distinct blocks left: shared, b's rewritten block 1, b's block 2, b's tail, c's block 1.
    assert_eq!(report.blocks, 5);
}

#[test]
fn verify_reports_corrupt_records() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());
    let pack = dir.path().join("packs").join("00000001.pack");
    let mut bytes = std::fs::read(&pack).unwrap();
    bytes[45] ^= 0x01;
    std::fs::write(&pack, bytes).unwrap();
    let report = open(dir.path()).verify().unwrap();
    assert!(!report.is_ok());
    assert!(
        report.problems.iter().any(|p| p.contains("checksum")),
        "{:#?}",
        report.problems
    );
}

#[test]
fn verify_reports_missing_pack_files() {
    let dir = tempfile::tempdir().unwrap();
    populated(dir.path());
    let store = open(dir.path());
    let victim = store
        .stats()
        .unwrap()
        .packs
        .iter()
        .find(|p| p.live_bytes > 0)
        .unwrap()
        .id;
    std::fs::remove_file(dir.path().join("packs").join(format!("{victim:08}.pack"))).unwrap();
    let report = store.verify().unwrap();
    assert!(
        report.problems.iter().any(|p| p.contains("file missing")),
        "{:#?}",
        report.problems
    );
}
```

Replace `src/lib.rs` with:

```rust
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
mod pack;
mod read;
mod stats;
mod store;
mod tracker;
mod verify;
mod write;

pub use config::{CompactOptions, StoreConfig};
pub use error::{Error, Result};
pub use stats::{IndexSize, PackStats, Stats};
pub use store::{BlockStore, FileInfo, ReadResult};
pub use verify::VerifyReport;
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test verify`
Expected: FAIL, compile error: `no method named verify found for struct BlockStore`.

- [ ] **Step 3: Write the implementation**

Create `src/verify.rs`:

```rust
//! Full consistency check (fsck).

use std::collections::HashMap;

use crate::codec::decode_payload;
use crate::error::Result;
use crate::index::{BlockLoc, PackState};
use crate::manifest::{
    MISSING, block_count, decode_ids, file_len, segment_count, slots_in_segment,
};
use crate::pack::pack_path;
use crate::store::BlockStore;

/// Result of [`BlockStore::verify`].
#[derive(Debug, Clone, Default)]
pub struct VerifyReport {
    pub files: u64,
    pub blocks: u64,
    /// Human-readable descriptions of every inconsistency found.
    pub problems: Vec<String>,
}

impl VerifyReport {
    pub fn is_ok(&self) -> bool {
        self.problems.is_empty()
    }
}

/// (file id, block count, next expected segment) of the file being checked.
type FileCursor = Option<(Vec<u8>, u64, u32)>;

fn finish_file(cur: &FileCursor, problems: &mut Vec<String>) {
    if let Some((id, blocks, next)) = cur
        && *next != segment_count(*blocks)
    {
        problems.push(format!(
            "file {:?}: has {next} segments, expected {}",
            String::from_utf8_lossy(id),
            segment_count(*blocks)
        ));
    }
}

impl BlockStore {
    /// Checks every manifest, refcount, dedup entry, pack total and record checksum.
    /// Reads every stored block, so it is slow on large stores. Memory use grows with the
    /// number of distinct referenced blocks.
    pub fn verify(&self) -> Result<VerifyReport> {
        let _guard = self.tracker.enter();
        let r = self.index.read()?;
        let bs = self.cfg.block_size;
        let mut report = VerifyReport::default();
        let problems = &mut report.problems;

        // Manifests: segments are complete and sized correctly; count references.
        let mut refs: HashMap<u64, u32> = HashMap::new();
        let mut current: FileCursor = None;
        let mut files = 0u64;
        r.for_each_segment(|id, seg, value| {
            if seg == 0 {
                finish_file(&current, problems);
                files += 1;
                current = Some((id.to_vec(), block_count(file_len(value), bs), 0));
            }
            match &mut current {
                Some((cur_id, blocks, next)) if cur_id.as_slice() == id && *next == seg => {
                    let ids = decode_ids(value, seg);
                    let expect = slots_in_segment(*blocks, seg);
                    if ids.len() != expect {
                        problems.push(format!(
                            "file {:?} segment {seg}: {} slots, expected {expect}",
                            String::from_utf8_lossy(id),
                            ids.len()
                        ));
                    }
                    for bid in ids.into_iter().filter(|&b| b != MISSING) {
                        *refs.entry(bid).or_default() += 1;
                    }
                    *next += 1;
                }
                _ => problems.push(format!(
                    "file {:?}: unexpected segment {seg}",
                    String::from_utf8_lossy(id)
                )),
            }
            Ok(())
        })?;
        finish_file(&current, problems);
        report.files = files;

        // Blocks: refcounts, dedup entries, records.
        let mut live: HashMap<u32, u64> = HashMap::new();
        let mut blocks = 0u64;
        r.for_each_block(|id, loc| {
            blocks += 1;
            let n = refs.remove(&id).unwrap_or(0);
            if n != loc.refcount {
                problems.push(format!(
                    "block {id}: refcount {} but {n} references",
                    loc.refcount
                ));
            }
            if r.dedup(&loc.hash)? != Some(id) {
                problems.push(format!("block {id}: dedup entry missing or wrong"));
            }
            *live.entry(loc.pack).or_default() += loc.record_len();
            let bad = match self.check_record(&loc) {
                Ok(Ok(())) => None,
                Ok(Err(reason)) => Some(reason.to_string()),
                Err(e) => Some(e.to_string()),
            };
            if let Some(reason) = bad {
                problems.push(format!(
                    "block {id} (pack {} offset {}): {reason}",
                    loc.pack, loc.offset
                ));
            }
            Ok(())
        })?;
        report.blocks = blocks;
        // Ids left in `refs` have no block row: healed blocks, which read as missing. Not an error.

        r.for_each_dedup(|hash, id| {
            if r.block(id)?.is_none_or(|l| l.hash != hash) {
                problems.push(format!("dedup entry for block {id} has no matching block"));
            }
            Ok(())
        })?;

        for (id, info) in r.packs()? {
            if info.state == PackState::Retired {
                continue;
            }
            let expect = live.remove(&id).unwrap_or(0);
            if info.live_bytes != expect {
                problems.push(format!(
                    "pack {id}: live_bytes {} but blocks total {expect}",
                    info.live_bytes
                ));
            }
            if !pack_path(&self.pack_dir, id).exists() {
                problems.push(format!("pack {id}: file missing"));
            }
        }
        for id in live.keys() {
            problems.push(format!(
                "blocks reference pack {id}, which is not registered or is retired"
            ));
        }
        Ok(report)
    }

    fn check_record(&self, loc: &BlockLoc) -> Result<std::result::Result<(), &'static str>> {
        let mut out = vec![0u8; loc.raw_len as usize];
        self.read_record(loc, |header, payload| {
            decode_payload(header, payload, &mut out)
        })
    }
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test verify`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs src/verify.rs tests/verify.rs
git commit -m "feat: verify() consistency checker"
```

---

### Task 12: Compaction and retired-pack deletion

**Files:**
- Create: `tests/compact.rs`
- Create: `src/compact.rs`
- Modify: `src/lib.rs` (full new content below)

`compact(opts)` takes `compact_lock`, deletes retired packs that are safe to delete, picks sealed
packs with `garbage_ratio() >= min_garbage_ratio` (worst first, up to `max_bytes`), and for each
pack does the following:

1. Scans it sequentially, stopping at the first undecodable header, which is a torn tail.
2. In batches: keeps records whose `dedup[hash] -> blocks[id]` still points at this pack and offset, heals any whose checksum fails, appends the rest unchanged to the active pack, and repoints the `blocks` rows in one transaction. A row that moved or was freed meanwhile is skipped, and its copy becomes garbage.
3. Evacuates any block still recorded in this pack by scanning the index. This covers records that step 1 could not reach because of a corrupt header.
4. Marks the pack `Retired` in a `durable_commit`. This requires `live_bytes == 0`.
5. Calls `tracker.advance()` and queues the pack for deletion once older reads finish.

Deletion closes the cached handle first, which Windows requires. If deletion fails (for example
because a virus scanner holds the file), the pack stays queued, and `open` deletes any retired pack
anyway.

**Interfaces:**
- Consumes: `BlockStore::{stats, append_records, durable_commit, heal, tracker, retired, compact_lock, packs}`, `Tables::{block, put_block, add_live, pack, put_pack, remove_pack}`, `pack::{pack_path, remove_pack_file}`, `crash::point`.
- Produces: `BlockStore::compact(&self, opts: CompactOptions) -> Result<CompactReport>`, `pub struct CompactReport { packs_compacted: u32, bytes_read: u64, bytes_moved: u64 }` and `pub(crate) fn delete_retired(&self) -> Result<()>`.

- [ ] **Step 1: Write the failing tests**

Create `tests/compact.rs`:

```rust
mod common;

use block_store::CompactOptions;
use common::*;

fn fill(store: &block_store::BlockStore, files: u64) -> Vec<Vec<u8>> {
    (0..files)
        .map(|i| {
            let data = random_bytes(100 + i, 4 * BS);
            let id = format!("f{i}");
            store.set_len(id.as_bytes(), data.len() as u64).unwrap();
            store.write_blocks(id.as_bytes(), 0, &data).unwrap();
            data
        })
        .collect()
}

#[test]
fn compaction_reclaims_space_and_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let data = fill(&store, 40);
    for i in (0..40).filter(|i| i % 4 != 0) {
        store.delete(format!("f{i}").as_bytes()).unwrap();
    }
    let before = store.stats().unwrap();
    let before_bytes: u64 = before.packs.iter().map(|p| p.file_bytes).sum();

    let report = store.compact(CompactOptions::default()).unwrap();
    assert!(report.packs_compacted > 0);

    let after = store.stats().unwrap();
    let after_bytes: u64 = after.packs.iter().map(|p| p.file_bytes).sum();
    assert!(
        after_bytes < before_bytes / 2,
        "{after_bytes} vs {before_bytes}"
    );
    for i in (0..40).filter(|i| i % 4 == 0) {
        assert_eq!(
            read_all(&store, format!("f{i}").as_bytes()),
            data[i as usize]
        );
    }
    assert!(store.verify().unwrap().is_ok());
    let on_disk = std::fs::read_dir(dir.path().join("packs")).unwrap().count();
    assert_eq!(on_disk, after.packs.len());
}

#[test]
fn compaction_respects_threshold_and_budget() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    fill(&store, 40);
    // Nothing deleted: no pack qualifies.
    assert_eq!(
        store
            .compact(CompactOptions::default())
            .unwrap()
            .packs_compacted,
        0
    );
    for i in 0..40 {
        store.delete(format!("f{i}").as_bytes()).unwrap();
    }
    let one = store
        .compact(CompactOptions {
            min_garbage_ratio: 0.5,
            max_bytes: 1,
        })
        .unwrap();
    assert_eq!(one.packs_compacted, 1);
    assert_eq!(one.bytes_moved, 0);
}

#[test]
fn compacted_store_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let data = {
        let store = open(dir.path());
        let data = fill(&store, 20);
        for i in 1..20 {
            store.delete(format!("f{i}").as_bytes()).unwrap();
        }
        store.compact(CompactOptions::default()).unwrap();
        store.close().unwrap();
        data
    };
    let store = open(dir.path());
    assert_eq!(read_all(&store, b"f0"), data[0]);
    assert!(store.verify().unwrap().is_ok());
}

#[test]
fn reads_and_writes_during_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let data = fill(&store, 60);
    for i in (0..60).filter(|i| i % 3 != 0) {
        store.delete(format!("f{i}").as_bytes()).unwrap();
    }
    std::thread::scope(|s| {
        let store = &store;
        let data = &data;
        for t in 0..3 {
            s.spawn(move || {
                for round in 0..20 {
                    let i = ((round * 3 + t) % 20) * 3;
                    assert_eq!(read_all(store, format!("f{i}").as_bytes()), data[i]);
                }
            });
        }
        s.spawn(move || {
            for i in 0..10u64 {
                let id = format!("new{i}");
                let d = random_bytes(5000 + i, 2 * BS);
                store.set_len(id.as_bytes(), d.len() as u64).unwrap();
                store.write_blocks(id.as_bytes(), 0, &d).unwrap();
            }
        });
        s.spawn(move || {
            store.compact(CompactOptions::default()).unwrap();
        });
    });
    store.compact(CompactOptions::default()).unwrap();
    assert!(store.verify().unwrap().is_ok());
    for i in (0..60).filter(|i| i % 3 == 0) {
        assert_eq!(read_all(&store, format!("f{i}").as_bytes()), data[i]);
    }
}
```

Replace `src/lib.rs` with:

```rust
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
```

- [ ] **Step 2: Run the tests and confirm they fail**

Run: `cargo test --test compact`
Expected: FAIL, compile error: `no method named compact found for struct BlockStore`.

- [ ] **Step 3: Write the implementation**

Create `src/compact.rs`:

```rust
//! Compaction: copy live records out of mostly-garbage packs, then retire and delete them.

use std::fs::File;
use std::io::{self, BufReader, Read};

use crate::codec::{HEADER_LEN, RecordHeader, checksum};
use crate::config::CompactOptions;
use crate::crash;
use crate::error::{Error, Result};
use crate::index::{BlockLoc, PackState};
use crate::pack::{pack_path, remove_pack_file};
use crate::store::BlockStore;

/// What one [`BlockStore::compact`] call did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactReport {
    pub packs_compacted: u32,
    /// Pack bytes scanned.
    pub bytes_read: u64,
    /// Live record bytes copied into the active pack.
    pub bytes_moved: u64,
}

/// A live record to move: its block id, current location, and raw record bytes.
struct LiveRecord {
    id: u64,
    loc: BlockLoc,
    bytes: Vec<u8>,
}

/// A record found while scanning a pack, not yet checked for liveness.
struct Scanned {
    offset: u64,
    header: RecordHeader,
    bytes: Vec<u8>,
}

impl BlockStore {
    /// Compacts sealed packs whose garbage ratio is at least `opts.min_garbage_ratio`, worst first,
    /// until `opts.max_bytes` pack bytes have been processed. Readers and writers keep working.
    pub fn compact(&self, opts: CompactOptions) -> Result<CompactReport> {
        let _serial = self.compact_lock.lock().unwrap();
        self.delete_retired()?;
        let mut candidates = Vec::new();
        for p in self.stats()?.packs {
            if p.sealed && p.file_bytes > 0 && p.garbage_ratio() >= opts.min_garbage_ratio {
                candidates.push(p);
            }
        }
        candidates.sort_by(|a, b| b.garbage_ratio().total_cmp(&a.garbage_ratio()));

        let mut report = CompactReport::default();
        for p in candidates {
            if report.bytes_read >= opts.max_bytes {
                break;
            }
            report.bytes_moved += self.compact_pack(p.id)?;
            report.bytes_read += p.file_bytes;
            report.packs_compacted += 1;
        }
        self.delete_retired()?;
        Ok(report)
    }

    /// Moves every live record out of `pack` and retires it. Returns bytes moved.
    fn compact_pack(&self, pack: u32) -> Result<u64> {
        let file = File::open(pack_path(&self.pack_dir, pack))?;
        let size = file.metadata()?.len();
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut offset = 0u64;
        let mut moved = 0u64;
        let mut batch = Vec::new();
        let mut batch_bytes = 0usize;

        // Sequential scan. Stops at the first unreadable header (a torn tail after a crash);
        // anything live beyond it is picked up by `evacuate_by_index` below.
        while offset + HEADER_LEN as u64 <= size {
            let mut bytes = vec![0u8; HEADER_LEN];
            reader.read_exact(&mut bytes)?;
            let Ok(header) = RecordHeader::decode(&bytes) else {
                break;
            };
            if offset + header.record_len() > size {
                break;
            }
            bytes.resize(header.record_len() as usize, 0);
            reader.read_exact(&mut bytes[HEADER_LEN..])?;
            batch_bytes += bytes.len();
            batch.push(Scanned {
                offset,
                header,
                bytes,
            });
            offset += header.record_len();
            if batch_bytes >= self.cfg.write_txn_bytes {
                moved += self.move_scanned(pack, std::mem::take(&mut batch))?;
                batch_bytes = 0;
            }
        }
        moved += self.move_scanned(pack, batch)?;
        moved += self.evacuate_by_index(pack)?;

        crash::point("compact_before_retire");
        self.durable_commit(|t| {
            let mut info = t
                .pack(pack)?
                .ok_or_else(|| Error::Corrupt(format!("pack {pack} vanished during compaction")))?;
            if info.live_bytes != 0 {
                return Err(Error::Corrupt(format!(
                    "pack {pack} still has live data after compaction"
                )));
            }
            info.state = PackState::Retired;
            t.put_pack(pack, &info)
        })?;
        let generation = self.tracker.advance();
        self.retired.lock().unwrap().push((pack, generation));
        crash::point("compact_after_retire");
        Ok(moved)
    }

    /// Keeps the scanned records that are still the live copy of their block, then moves them.
    fn move_scanned(&self, pack: u32, scanned: Vec<Scanned>) -> Result<u64> {
        let mut live = Vec::new();
        {
            let r = self.index.read()?;
            for s in scanned {
                let Some(id) = r.dedup(&s.header.hash)? else {
                    continue;
                };
                let Some(loc) = r.block(id)? else { continue };
                if loc.pack != pack || loc.offset != s.offset {
                    continue;
                }
                if checksum(&s.bytes[HEADER_LEN..]) != s.header.checksum
                    || s.header.stored_len != loc.stored_len
                {
                    self.heal(id, &loc, "checksum mismatch during compaction")?;
                    continue;
                }
                live.push(LiveRecord {
                    id,
                    loc,
                    bytes: s.bytes,
                });
            }
        }
        self.move_records(live)
    }

    /// Moves any block still recorded in `pack` by looking it up in the index. Normally finds
    /// nothing; it covers records the sequential scan could not reach (corrupt headers).
    fn evacuate_by_index(&self, pack: u32) -> Result<u64> {
        let mut locs = Vec::new();
        self.index.read()?.for_each_block(|id, loc| {
            if loc.pack == pack {
                locs.push((id, loc));
            }
            Ok(())
        })?;
        let mut moved = 0;
        for chunk in locs.chunks(256) {
            let mut live = Vec::new();
            for &(id, loc) in chunk {
                let mut bytes = vec![0u8; loc.record_len() as usize];
                let ok = match self.packs.read_exact_at(pack, &mut bytes, loc.offset) {
                    Ok(()) => RecordHeader::decode(&bytes[..HEADER_LEN]).is_ok_and(|h| {
                        h.hash == loc.hash
                            && h.stored_len == loc.stored_len
                            && checksum(&bytes[HEADER_LEN..]) == h.checksum
                    }),
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => false,
                    Err(e) => return Err(e.into()),
                };
                if ok {
                    live.push(LiveRecord { id, loc, bytes });
                } else {
                    self.heal(id, &loc, "unreadable record during compaction")?;
                }
            }
            moved += self.move_records(live)?;
        }
        Ok(moved)
    }

    /// Appends records to the active pack and repoints their blocks, skipping any block that
    /// was freed or moved meanwhile (its copy becomes garbage).
    fn move_records(&self, live: Vec<LiveRecord>) -> Result<u64> {
        if live.is_empty() {
            return Ok(0);
        }
        let new_locs = self.append_records(
            live.iter()
                .map(|l| (&l.bytes[..HEADER_LEN], &l.bytes[HEADER_LEN..])),
        )?;
        crash::point("compact_after_copy");
        self.index.update(false, |t| {
            let mut moved = 0u64;
            for (l, (new_pack, new_offset)) in live.iter().zip(new_locs) {
                let Some(cur) = t.block(l.id)? else { continue };
                if cur.pack != l.loc.pack || cur.offset != l.loc.offset {
                    continue;
                }
                let len = cur.record_len() as i64;
                t.put_block(
                    l.id,
                    &BlockLoc {
                        pack: new_pack,
                        offset: new_offset,
                        ..cur
                    },
                )?;
                t.add_live(cur.pack, -len)?;
                t.add_live(new_pack, len)?;
                moved += len as u64;
            }
            Ok(moved)
        })
    }

    /// Deletes retired packs that no running read can still reference.
    pub(crate) fn delete_retired(&self) -> Result<()> {
        let pending = std::mem::take(&mut *self.retired.lock().unwrap());
        let mut keep = Vec::new();
        for (pack, generation) in pending {
            if !self.tracker.is_clear_before(generation) {
                keep.push((pack, generation));
                continue;
            }
            self.packs.close(pack);
            if let Err(e) = remove_pack_file(&self.pack_dir, pack) {
                // Windows: another program (for example a virus scanner) may hold the file.
                tracing::warn!(pack, error = %e, "could not delete retired pack; will retry");
                keep.push((pack, generation));
                continue;
            }
            self.index.update(false, |t| t.remove_pack(pack))?;
        }
        self.retired.lock().unwrap().extend(keep);
        Ok(())
    }
}
```

- [ ] **Step 4: Run the tests and confirm they pass**

Run: `cargo test --test compact`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/compact.rs src/lib.rs tests/compact.rs
git commit -m "feat: online compaction with deferred pack deletion"
```

---

### Task 13: Model-based property test

**Files:**
- Create: `tests/model.rs`

Random sequences of `set_len`, `write_blocks`, `delete`, `compact` (with ratio 0, so every sealed
pack is rewritten), `flush` and close-then-reopen, run against an in-memory model. Only six
distinct block seeds are used, so dedup happens constantly; even seeds compress and odd seeds do
not. After **every** operation, reads, `missing` and `cached_ranges` must match the model, and
`verify()` must pass.

This task adds verification of behavior that already exists, so the test is expected to pass
immediately. A failure is a real bug: use superpowers:systematic-debugging and get proptest's
minimized case before changing any code.

**Interfaces:**
- Consumes: the whole public API and `tests/common`.
- Produces: nothing new.

- [ ] **Step 1: Write the tests**

Create `tests/model.rs`:

```rust
//! Model-based property test: random operation sequences against an in-memory model.

mod common;

use std::collections::HashMap;
use std::ops::Range;

use block_store::{BlockStore, CompactOptions, Error};
use common::*;
use proptest::prelude::*;

const FILES: u8 = 4;
const MAX_BLOCKS: u64 = 6;

#[derive(Debug, Clone)]
enum Op {
    SetLen {
        file: u8,
        len: u64,
    },
    Write {
        file: u8,
        first: u64,
        seeds: Vec<u8>,
    },
    Delete {
        file: u8,
    },
    Compact,
    Flush,
    Reopen,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..FILES, 0..=MAX_BLOCKS * BS as u64).prop_map(|(file, len)| Op::SetLen { file, len }),
        6 => (0..FILES, 0..MAX_BLOCKS, prop::collection::vec(0..6u8, 1..6))
            .prop_map(|(file, first, seeds)| Op::Write { file, first, seeds }),
        1 => (0..FILES).prop_map(|file| Op::Delete { file }),
        1 => Just(Op::Compact),
        1 => Just(Op::Flush),
        1 => Just(Op::Reopen),
    ]
}

/// Model of one file: length and per-block content (`None` = not cached).
#[derive(Debug, Clone)]
struct ModelFile {
    len: u64,
    blocks: Vec<Option<Vec<u8>>>,
}

fn block_len(len: u64, idx: u64) -> usize {
    len.saturating_sub(idx * BS as u64).min(BS as u64) as usize
}

/// Few distinct seeds so dedup happens; even seeds compress, odd seeds do not.
fn content(seed: u8, len: usize) -> Vec<u8> {
    if seed.is_multiple_of(2) {
        pattern_bytes(seed as u64, len)
    } else {
        random_bytes(seed as u64, len)
    }
}

fn file_id(f: u8) -> [u8; 2] {
    [b'f', f]
}

fn resize(m: &mut ModelFile, new_len: u64) {
    let nb = new_len.div_ceil(BS as u64) as usize;
    let common = m.blocks.len().min(nb);
    if common > 0 && block_len(m.len, common as u64 - 1) != block_len(new_len, common as u64 - 1) {
        m.blocks[common - 1] = None;
    }
    m.blocks.resize(nb, None);
    m.len = new_len;
}

fn apply(
    store: &mut Option<BlockStore>,
    dir: &std::path::Path,
    model: &mut HashMap<u8, ModelFile>,
    op: &Op,
) {
    let s = store.as_ref().unwrap();
    match op {
        Op::SetLen { file, len } => {
            s.set_len(&file_id(*file), *len).unwrap();
            let m = model.entry(*file).or_insert(ModelFile {
                len: 0,
                blocks: Vec::new(),
            });
            resize(m, *len);
        }
        Op::Write { file, first, seeds } => {
            let Some(m) = model.get_mut(file) else {
                assert!(matches!(
                    s.write_blocks(&file_id(*file), 0, &[0u8; BS]),
                    Err(Error::NotFound)
                ));
                return;
            };
            let nb = m.blocks.len() as u64;
            if nb == 0 {
                assert!(matches!(
                    s.write_blocks(&file_id(*file), 0, &[0u8; BS]),
                    Err(Error::OutOfRange)
                ));
                return;
            }
            let first = first % nb;
            let count = (seeds.len() as u64).min(nb - first);
            let mut data = Vec::new();
            for k in 0..count {
                let idx = first + k;
                let block = content(seeds[k as usize], block_len(m.len, idx));
                data.extend_from_slice(&block);
                m.blocks[idx as usize] = Some(block);
            }
            s.write_blocks(&file_id(*file), first, &data).unwrap();
        }
        Op::Delete { file } => {
            let r = s.delete(&file_id(*file));
            if model.remove(file).is_some() {
                r.unwrap();
            } else {
                assert!(matches!(r, Err(Error::NotFound)));
            }
        }
        Op::Compact => {
            s.compact(CompactOptions {
                min_garbage_ratio: 0.0,
                max_bytes: u64::MAX,
            })
            .unwrap();
        }
        Op::Flush => s.flush().unwrap(),
        Op::Reopen => {
            store.take().unwrap().close().unwrap();
            *store = Some(open(dir));
        }
    }
}

fn check(store: &BlockStore, model: &HashMap<u8, ModelFile>) {
    for f in 0..FILES {
        let id = file_id(f);
        let Some(m) = model.get(&f) else {
            assert!(store.stat(&id).unwrap().is_none());
            continue;
        };
        assert_eq!(store.stat(&id).unwrap().unwrap().len, m.len);
        let mut buf = vec![0u8; m.len as usize];
        let r = store.read(&id, 0, &mut buf).unwrap();
        assert_eq!(r.bytes, m.len as usize);

        let mut expect_missing: Vec<Range<u64>> = Vec::new();
        let mut expect_cached: Vec<Range<u64>> = Vec::new();
        for (i, b) in m.blocks.iter().enumerate() {
            let start = i as u64 * BS as u64;
            let range = start..start + block_len(m.len, i as u64) as u64;
            let list = if b.is_some() {
                &mut expect_cached
            } else {
                &mut expect_missing
            };
            match list.last_mut() {
                Some(last) if last.end == range.start => last.end = range.end,
                _ => list.push(range.clone()),
            }
            if let Some(b) = b {
                assert!(
                    buf[range.start as usize..range.end as usize] == b[..],
                    "file {f} block {i}: wrong bytes"
                );
            }
        }
        assert_eq!(r.missing, expect_missing, "file {f}");
        assert_eq!(store.cached_ranges(&id).unwrap(), expect_cached, "file {f}");
    }
    let report = store.verify().unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    #[test]
    fn store_matches_model(ops in prop::collection::vec(op(), 1..40)) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Some(open(dir.path()));
        let mut model = HashMap::new();
        for op in &ops {
            apply(&mut store, dir.path(), &mut model, op);
            check(store.as_ref().unwrap(), &model);
        }
    }
}
```

- [ ] **Step 2: Run the tests and confirm they pass**

Run: `cargo test --release --test model`
Expected: PASS.

- [ ] **Step 3: Run a longer search once**

Run: `PROPTEST_CASES=1000 cargo test --release --test model` (in PowerShell: `$env:PROPTEST_CASES=1000; cargo test --release --test model`)
Expected: PASS in about 30 s.

- [ ] **Step 4: Check that the test can fail**

Temporarily change `if loc.refcount > 1 {` in `Tables::decref` (`src/index.rs`) to `if loc.refcount > 2 {`, run `cargo test --release --test model`, and confirm it FAILS with a "wrong bytes" or verify problem. Revert the change and confirm it passes again.

- [ ] **Step 5: Commit**

```bash
git add tests/model.rs
git commit -m "test: model-based property test against an in-memory model"
```

---

### Task 14: Crash tests in a child process

**Files:**
- Create: `tests/crash.rs`

Each test re-runs the test binary as a child process (`child_entry`). The child creates and
flushes file "a", arms `BLOCK_STORE_CRASH_AT`, and runs a scenario until it aborts at a named crash
point: `write_after_append`, `flush_before_commit`, `compact_after_copy`, `compact_before_retire`
or `compact_after_retire`. The parent then reopens the store and checks the following:

- "a" reads back intact.
- `verify()` passes.
- Nothing unflushed reads back as wrong bytes.
- A retired pack is gone after reopen.
- The store keeps working after the crash.

The crash points were added in Tasks 8, 9 and 12. The tests are expected to pass. A failure is a
real recovery bug: this suite found one while the plan was being prepared, where `recover` deleted
a retired pack's file and then tried to delete it again as an orphan.

**Interfaces:**
- Consumes: `crash-points` feature, public API, `tests/common`.
- Produces: nothing new.

- [ ] **Step 1: Write the tests**

Create `tests/crash.rs`:

```rust
//! Crash tests: a child process runs a scenario and aborts at a named crash point; the parent
//! reopens the store and checks that flushed data survived and nothing reads back wrong.
//! Run with: cargo test --features crash-points --test crash
#![cfg(feature = "crash-points")]

mod common;

use std::path::Path;
use std::process::Command;

use block_store::{BlockStore, CompactOptions};
use common::*;

const ROLE: &str = "BLOCK_STORE_CRASH_ROLE";
const DIR: &str = "BLOCK_STORE_CRASH_DIR";
/// Crash point the child arms once its setup is done.
const POINT: &str = "BLOCK_STORE_CRASH_POINT";
/// Read by the library's crash points.
const CRASH_AT: &str = "BLOCK_STORE_CRASH_AT";

/// Durable baseline: file "a" (flushed). Then unflushed file "b".
fn setup(store: &BlockStore) {
    let a = random_bytes(1, 6 * BS);
    store.set_len(b"a", a.len() as u64).unwrap();
    store.write_blocks(b"a", 0, &a).unwrap();
    store.flush().unwrap();
}

fn scenario(name: &str, dir: &Path) {
    let store = open(dir);
    setup(&store);
    // SAFETY: the child runs a single test thread and no other thread reads the environment
    // concurrently; crash points only read this variable.
    unsafe { std::env::set_var(CRASH_AT, std::env::var(POINT).unwrap()) };
    match name {
        "write" => {
            let b = random_bytes(2, 3 * BS);
            store.set_len(b"b", b.len() as u64).unwrap();
            store.write_blocks(b"b", 0, &b).unwrap();
        }
        "flush" => {
            let b = random_bytes(2, 3 * BS);
            store.set_len(b"b", b.len() as u64).unwrap();
            store.write_blocks(b"b", 0, &b).unwrap();
            store.flush().unwrap();
        }
        "compact" => {
            // Fill several packs, delete most of it, compact.
            for i in 0..30u64 {
                let id = format!("junk{i}");
                store.set_len(id.as_bytes(), 4 * BS as u64).unwrap();
                store
                    .write_blocks(id.as_bytes(), 0, &random_bytes(100 + i, 4 * BS))
                    .unwrap();
            }
            store.flush().unwrap();
            for i in 0..30u64 {
                store.delete(format!("junk{i}").as_bytes()).unwrap();
            }
            store.compact(CompactOptions::default()).unwrap();
        }
        other => panic!("unknown scenario {other}"),
    }
    // Scenarios must crash before reaching here.
    std::process::exit(3);
}

/// Runs `scenario` in a child process that aborts at `point`, then returns the store directory.
fn crash_child(scenario: &str, point: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env(ROLE, scenario)
        .env(DIR, dir.path())
        .env(POINT, point)
        .env_remove(CRASH_AT)
        .status()
        .unwrap();
    assert!(!status.success(), "child should have aborted at {point}");
    assert_ne!(
        status.code(),
        Some(3),
        "child never reached crash point {point}"
    );
    dir
}

/// Entry point for the child process; does nothing in a normal test run.
#[test]
fn child_entry() {
    if let (Ok(role), Ok(dir)) = (std::env::var(ROLE), std::env::var(DIR)) {
        scenario(&role, Path::new(&dir));
    }
}

fn check_after_crash(dir: &Path) -> BlockStore {
    let store = open(dir);
    assert_eq!(
        read_all(&store, b"a"),
        random_bytes(1, 6 * BS),
        "flushed data lost"
    );
    let report = store.verify().unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    store
}

#[test]
fn crash_after_append_before_commit() {
    let dir = crash_child("write", "write_after_append");
    let store = check_after_crash(dir.path());
    // "b" may exist (set_len commits are not durable, so usually not); if it does, it must not
    // return wrong bytes.
    if let Some(info) = store.stat(b"b").unwrap() {
        let mut buf = vec![0u8; info.len as usize];
        let r = store.read(b"b", 0, &mut buf).unwrap();
        let b = random_bytes(2, 3 * BS);
        for block in 0..3 {
            let range = (block * BS) as u64..((block + 1) * BS) as u64;
            if !r
                .missing
                .iter()
                .any(|m| m.start <= range.start && range.end <= m.end)
            {
                assert!(buf[block * BS..(block + 1) * BS] == b[block * BS..(block + 1) * BS]);
            }
        }
    }
}

#[test]
fn crash_during_flush_before_durable_commit() {
    let dir = crash_child("flush", "flush_before_commit");
    let store = check_after_crash(dir.path());
    // The durable commit never happened, so "b" is not there.
    assert!(store.stat(b"b").unwrap().is_none());
}

#[test]
fn crash_during_compaction_after_copy() {
    let dir = crash_child("compact", "compact_after_copy");
    check_after_crash(dir.path());
}

#[test]
fn crash_during_compaction_before_retire() {
    let dir = crash_child("compact", "compact_before_retire");
    check_after_crash(dir.path());
}

#[test]
fn crash_during_compaction_after_retire() {
    let dir = crash_child("compact", "compact_after_retire");
    let store = check_after_crash(dir.path());
    // The retired pack was deleted on open.
    let on_disk = std::fs::read_dir(dir.path().join("packs")).unwrap().count();
    assert_eq!(on_disk, store.stats().unwrap().packs.len());
}

#[test]
fn store_is_usable_after_crash() {
    let dir = crash_child("write", "write_after_append");
    let store = check_after_crash(dir.path());
    let c = random_bytes(3, 2 * BS);
    store.set_len(b"c", c.len() as u64).unwrap();
    store.write_blocks(b"c", 0, &c).unwrap();
    store.flush().unwrap();
    drop(store);
    let store = check_after_crash(dir.path());
    assert_eq!(read_all(&store, b"c"), c);
}
```

- [ ] **Step 2: Run the tests and confirm they pass**

Run: `cargo test --features crash-points --test crash`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add tests/crash.rs
git commit -m "test: subprocess crash tests for writes, flush and compaction"
```

---

### Task 15: Benchmarks, scale test, CI, README

**Files:**
- Create: `benches/store.rs`
- Create: `tests/scale.rs`
- Create: `.github/workflows/ci.yml`
- Create: `README.md`
- Modify: `Cargo.toml` (full new content below)

Adds criterion benchmarks for the spec's performance claims, the opt-in scale test that checks
index size and memory, CI on all three platforms, and a README.

Reference numbers measured while preparing this plan (Windows 11 laptop, NVMe, release build):

| Benchmark | Result |
|---|---|
| `stat/scan_500000_files` | 0.65 s, about 1.3 µs per file (including `format!` of each id) |
| `read/warm_random_64k_block` | about 46-50 µs, 1.2-1.3 GiB/s |
| `write/16mib_compressible_new` | about 380 MiB/s |
| `write/16mib_all_dedup_hits` | about 8 GiB/s |
| `compact/256mib_half_garbage` | about 217 ms |
| scale test, 32 GiB | index pages about 103 bytes/block (76 of it keys and values); resident memory about 35 MiB plus the redb cache; about 260 MiB/s single-writer ingest |

**Interfaces:**
- Consumes: the public API (including `index_size`).
- Produces: `benches/store.rs`, `tests/scale.rs`, `.github/workflows/ci.yml` and `README.md`.

- [ ] **Step 1: Add the benchmark, scale test, CI workflow and README**

Create `benches/store.rs`:

```rust
//! Benchmarks. `BLOCK_STORE_BENCH_FILES` sets the file count for the stat scan (default 500000).

use std::hint::black_box;
use std::time::Duration;

use block_store::{BlockStore, CompactOptions, StoreConfig};
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};

const BS: usize = 64 * 1024;

fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    blake3::Hasher::new()
        .update(&seed.to_le_bytes())
        .finalize_xof()
        .fill(&mut out);
    out
}

/// Roughly 3:1 compressible data.
fn compressible_bytes(seed: u64, len: usize) -> Vec<u8> {
    let noise = random_bytes(seed, len / 4);
    (0..len)
        .map(|i| {
            if i % 4 == 0 {
                noise[i / 4]
            } else {
                (i / 256) as u8
            }
        })
        .collect()
}

fn open(dir: &std::path::Path) -> BlockStore {
    BlockStore::open(
        dir,
        StoreConfig {
            max_pack_size: 256 << 20,
            ..StoreConfig::default()
        },
    )
    .unwrap()
}

fn stat_scan(c: &mut Criterion) {
    let files: u64 = std::env::var("BLOCK_STORE_BENCH_FILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(500_000);
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    for i in 0..files {
        store
            .set_len(
                format!("mods/some-game/file-{i:08}.dat").as_bytes(),
                12345 + i,
            )
            .unwrap();
    }
    store.flush().unwrap();
    let mut g = c.benchmark_group("stat");
    g.sample_size(10).measurement_time(Duration::from_secs(20));
    g.throughput(Throughput::Elements(files));
    g.bench_function(format!("scan_{files}_files"), |b| {
        b.iter(|| {
            for i in 0..files {
                black_box(
                    store
                        .stat(format!("mods/some-game/file-{i:08}.dat").as_bytes())
                        .unwrap(),
                );
            }
        })
    });
    g.finish();
}

fn reads(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let blocks = 4096u64; // 256 MiB
    store.set_len(b"big", blocks * BS as u64).unwrap();
    for chunk in 0..blocks / 64 {
        store
            .write_blocks(b"big", chunk * 64, &compressible_bytes(chunk, 64 * BS))
            .unwrap();
    }
    store.flush().unwrap();
    let mut buf = vec![0u8; BS];
    let mut g = c.benchmark_group("read");
    g.throughput(Throughput::Bytes(BS as u64));
    let mut n = 0u64;
    g.bench_function("warm_random_64k_block", |b| {
        b.iter(|| {
            n = n
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let block = (n >> 33) % blocks;
            black_box(store.read(b"big", block * BS as u64, &mut buf).unwrap());
        })
    });
    g.finish();
}

fn writes(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let size = 16 * BS * 16; // 16 MiB
    let mut g = c.benchmark_group("write");
    g.sample_size(10).throughput(Throughput::Bytes(size as u64));
    let mut seed = 0u64;
    g.bench_function("16mib_compressible_new", |b| {
        b.iter_batched(
            || {
                seed += 1;
                (format!("w{seed}"), compressible_bytes(seed, size))
            },
            |(id, data)| {
                store.set_len(id.as_bytes(), data.len() as u64).unwrap();
                store.write_blocks(id.as_bytes(), 0, &data).unwrap();
            },
            BatchSize::LargeInput,
        )
    });
    let same = compressible_bytes(u64::MAX, size);
    store.set_len(b"dedup-src", size as u64).unwrap();
    store.write_blocks(b"dedup-src", 0, &same).unwrap();
    g.bench_function("16mib_all_dedup_hits", |b| {
        b.iter(|| {
            seed += 1;
            let id = format!("d{seed}");
            store.set_len(id.as_bytes(), size as u64).unwrap();
            store.write_blocks(id.as_bytes(), 0, &same).unwrap();
        })
    });
    g.finish();
}

fn compaction(c: &mut Criterion) {
    let mut g = c.benchmark_group("compact");
    g.sample_size(10);
    g.bench_function("256mib_half_garbage", |b| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().unwrap();
                let store = BlockStore::open(
                    dir.path(),
                    StoreConfig {
                        max_pack_size: 32 << 20,
                        ..StoreConfig::default()
                    },
                )
                .unwrap();
                for i in 0..64u64 {
                    let id = format!("f{i}");
                    store.set_len(id.as_bytes(), 64 * BS as u64).unwrap();
                    store
                        .write_blocks(id.as_bytes(), 0, &compressible_bytes(i, 64 * BS))
                        .unwrap();
                }
                for i in (0..64u64).step_by(2) {
                    store.delete(format!("f{i}").as_bytes()).unwrap();
                }
                store.flush().unwrap();
                (dir, store)
            },
            |(dir, store)| {
                black_box(
                    store
                        .compact(CompactOptions {
                            min_garbage_ratio: 0.3,
                            max_bytes: u64::MAX,
                        })
                        .unwrap(),
                );
                drop(store);
                drop(dir);
            },
            BatchSize::PerIteration,
        )
    });
    g.finish();
}

criterion_group!(benches, stat_scan, reads, writes, compaction);
criterion_main!(benches);
```

Create `tests/scale.rs`:

```rust
//! Opt-in scale test. Writes `BLOCK_STORE_SCALE_GB` GiB (default 100) of synthetic, partly
//! duplicated data into `BLOCK_STORE_SCALE_DIR` (default: a temp dir) and checks index size and
//! resident memory against the design estimates. `BLOCK_STORE_SCALE_CACHE_MB` sets the redb
//! cache (default 64).
//! Run with: cargo test --release --test scale -- --ignored --nocapture

use std::time::Instant;

use block_store::{BlockStore, StoreConfig};

const BS: usize = 64 * 1024;

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Half random, half zero: compresses to about 50%.
fn block(seed: u64) -> Vec<u8> {
    let mut out = vec![0u8; BS];
    blake3::Hasher::new()
        .update(&seed.to_le_bytes())
        .finalize_xof()
        .fill(&mut out[..BS / 2]);
    out
}

fn rss_mib() -> u64 {
    memory_stats::memory_stats().map_or(0, |m| m.physical_mem as u64 >> 20)
}

#[test]
#[ignore]
fn hundred_gigabytes() {
    let gib = env_or("BLOCK_STORE_SCALE_GB", 100);
    let cache_mib = env_or("BLOCK_STORE_SCALE_CACHE_MB", 64);
    let tmp;
    let dir = match std::env::var("BLOCK_STORE_SCALE_DIR") {
        Ok(d) => std::path::PathBuf::from(d),
        Err(_) => {
            tmp = tempfile::tempdir().unwrap();
            tmp.path().to_path_buf()
        }
    };
    let cfg = StoreConfig {
        index_cache_bytes: (cache_mib as usize) << 20,
        ..StoreConfig::default()
    };
    let store = BlockStore::open(&dir, cfg).unwrap();
    let file_blocks = 256u64; // 16 MiB files
    let files = gib * 1024 / 16;
    let start = Instant::now();
    let mut peak_rss = 0;
    for f in 0..files {
        let id = format!("file-{f:08}");
        let data: Vec<u8> = (0..file_blocks)
            // One block in eight repeats an earlier one, so dedup is exercised.
            .flat_map(|b| {
                let n = f * file_blocks + b;
                block(if n % 8 == 7 { n / 2 } else { n })
            })
            .collect();
        store.set_len(id.as_bytes(), data.len() as u64).unwrap();
        store.write_blocks(id.as_bytes(), 0, &data).unwrap();
        if f % 128 == 0 {
            peak_rss = peak_rss.max(rss_mib());
            eprintln!(
                "{} GiB written, {:.0}s, rss {} MiB",
                f * 16 / 1024,
                start.elapsed().as_secs_f64(),
                rss_mib()
            );
        }
    }
    store.flush().unwrap();
    let elapsed = start.elapsed();

    let blocks = files * file_blocks;
    let index = store.index_size().unwrap();
    eprintln!(
        "{gib} GiB in {:.0}s ({:.0} MiB/s); index pages {} MiB ({:.1} bytes/block, {:.1} stored); \
         peak rss {peak_rss} MiB; packs {}",
        elapsed.as_secs_f64(),
        (gib * 1024) as f64 / elapsed.as_secs_f64(),
        index.page_bytes >> 20,
        index.page_bytes as f64 / blocks as f64,
        index.stored_bytes as f64 / blocks as f64,
        store.stats().unwrap().packs.len(),
    );
    // Measured about 103 bytes/block of pages (76 stored); fail well above that.
    assert!(
        index.page_bytes / blocks < 160,
        "index uses {} bytes per block",
        index.page_bytes / blocks
    );
    // Measured: about 35 MiB baseline plus the redb cache. Allow 100 MiB of headroom.
    assert!(
        peak_rss < cache_mib + 135,
        "peak resident memory {peak_rss} MiB"
    );
}
```

Create `.github/workflows/ci.yml`:

```yaml
name: CI

on:
  push:
  pull_request:

jobs:
  test:
    strategy:
      fail-fast: false
      matrix:
        os: [windows-latest, macos-latest, ubuntu-latest]
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt
      - run: cargo fmt --check
      - run: cargo clippy --all-targets --all-features -- -D warnings
      - run: cargo test --all-features
      - run: cargo test --release --test model
        env:
          PROPTEST_CASES: "512"
      - run: cargo bench --no-run
```

Create `README.md`:

````markdown
# block-store

A deduplicating, compressing block store for caching virtual file system content.

- Files are identified by caller-chosen byte strings (up to 256 bytes by default) and stored as
  64 KiB blocks. Files can be fully cached, partly cached, or not cached at all.
- Blocks are deduplicated by content (BLAKE3-128) within and across files, and compressed with
  zstd (level 6 by default).
- Block data lives in a few large append-only pack files (4 GiB by default); metadata lives in a
  [redb](https://github.com/cberner/redb) database.
- One process opens a store at a time; any number of threads in that process can use it.

Design: [docs/superpowers/specs/2026-09-23-block-store-design.md](docs/superpowers/specs/2026-09-23-block-store-design.md)

## Usage

```rust
use block_store::{BlockStore, CompactOptions, StoreConfig};

let store = BlockStore::open("cache-dir", StoreConfig::default())?;

// Create a 200 KiB file with every block missing, then cache its first two blocks.
store.set_len(b"mods/42/data.pak", 200 * 1024)?;
store.write_blocks(b"mods/42/data.pak", 0, &first_128_kib)?;

// Reads fill in cached bytes and report the ranges the caller must fetch.
let mut buf = vec![0u8; 200 * 1024];
let result = store.read(b"mods/42/data.pak", 0, &mut buf)?;
for range in result.missing {
    // fetch `range` from the source, then write_blocks() it
}

store.flush()?; // make everything so far durable
store.delete(b"mods/42/data.pak")?;
store.compact(CompactOptions::default())?; // reclaim space from deleted data
```

Writes must be block-aligned: every block is exactly `block_size` bytes except a file's final
block, which is exactly its remaining length.

## Durability

Writes become durable at `flush()`, at `close()`, and automatically after about 1 GiB of new
data. After a crash the store reopens consistent: everything flushed is readable and no read
returns bytes that were not written to that block. A corrupt block is dropped from the index and
reported as missing, so the caller fetches it again.

## Testing

```bash
cargo test --all-features                               # unit, integration, model and crash tests
PROPTEST_CASES=1000 cargo test --release --test model   # longer property test run
cargo test --release --test scale -- --ignored --nocapture  # 100 GiB scale test (BLOCK_STORE_SCALE_GB to change)
cargo bench                                             # criterion benchmarks
```
````

Replace `Cargo.toml` with:

```toml
[package]
name = "block-store"
version = "0.1.0"
edition = "2024"
rust-version = "1.89"
description = "Deduplicating, compressing block store for a virtual file system cache"
license = "MIT OR Apache-2.0"

[dependencies]
blake3 = "1.8"
rayon = "1.12"
redb = "4.3"
thiserror = "2"
tracing = "0.1"
xxhash-rust = { version = "0.8", features = ["xxh3"] }
zstd = "0.14"

[dev-dependencies]
criterion = "0.8"
proptest = "1.11"
memory-stats = "1.2"
tempfile = "3.27"

[features]
# Enables crash_point() calls that abort the process when BLOCK_STORE_CRASH_AT matches.
crash-points = []

[[bench]]
name = "store"
harness = false
```

- [ ] **Step 2: Run the tests and confirm they pass**

Run: `cargo bench --no-run && cargo test --release --test scale -- --ignored --nocapture`
Set `BLOCK_STORE_SCALE_GB=4` for a quick scale run (in PowerShell: `$env:BLOCK_STORE_SCALE_GB=4`). The full 100 GiB default takes about 7 minutes and needs about 60 GiB of free disk.
Expected: PASS.

- [ ] **Step 3: Run the full suite and the CI checks locally**

Run each and confirm they succeed with no warnings:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

Expected: formatting clean, no clippy findings, all tests pass (64 passed, 1 ignored: the scale test).

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/ci.yml Cargo.toml README.md benches/store.rs tests/scale.rs
git commit -m "chore: benchmarks, scale test, CI and README"
```

---

## Done

The store is complete when Task 15's full-suite step passes on Windows, macOS and Linux (CI). Follow-ups deliberately left out (see spec non-goals): background compaction, recompressing at a new zstd level, merging small packs, readahead, and an `Index` trait for swapping redb (the module boundary already confines redb to `src/index.rs`, so extracting a trait later is mechanical).
