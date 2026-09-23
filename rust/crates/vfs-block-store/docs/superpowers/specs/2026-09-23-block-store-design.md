# Block Store Design

Date: 2026-09-23
Status: Draft for review

## 1. Purpose

A Rust library crate that stores and caches file content for a virtual file system. It maps
user-defined file ids to file content, stored as 64 KiB blocks with block-level deduplication
and zstd compression, packed into a small number of large pack files.

### Requirements

- **Mapping:** `file_id` (user-defined bytes: hash, GUID, app value; capped at 256 bytes) -> file content.
- **Sparse caching:** a file may be fully cached, partially cached, or not cached at all. Reads
  report which ranges are missing; the caller fetches and writes them.
- **Deduplication:** block level, within and across files.
- **Compression:** zstd per block, level configurable (default 6).
- **Mutability:** data is mostly write-once; under 1% is mutable, usually rewritten in bulk.
  Copy-on-write at block granularity.
- **Durability:** best-effort. Explicit `flush()` makes all prior writes durable. After a crash
  the store reopens consistent and never returns wrong data.
- **Pack files:** configurable maximum size (default 4 GiB), so there are few files on disk.
- **Scale:** total dataset usually 100 GB to 1 TB (about 1.6M to 16M blocks). A startup scan of
  500,000 files must be fast.
- **Memory:** small, bounded resident memory. The index lives on disk.
- **Platforms:** Windows, macOS, Linux.
- **Process model:** single process, multi-threaded. The store is exclusively locked by one process.
- **Deletion policy:** the application decides. The store never evicts on its own; it only reclaims
  space from data the application deleted or overwrote.

### Non-goals

- Multi-process access
- Readahead, prefetching, or a decompressed-block cache (the OS page cache holds compressed data)
- LRU or capacity-based eviction
- Unaligned writes (the VFS handles those cases itself)
- Background auto-compaction, recompression at a new zstd level, merging small packs
- Rebuilding the index from pack files (if `index.redb` is lost, the cache is lost)

## 2. Architecture

### Components

| Unit | Responsibility | Depends on |
|---|---|---|
| `BlockStore` | Public API, validation, orchestration | all below |
| `Index` (trait) + `RedbIndex` | All metadata: manifests, block locations, dedup, pack accounting. The only module that touches redb. | redb |
| `PackWriter` | Owns the single active pack: appends records, rotates at `max_pack_size` | `Codec` |
| `PackReader` | Positioned reads of records by (pack, offset); keeps pack handles open | `Codec` |
| `Codec` | zstd compress/decompress, BLAKE3-128 hashing, xxh3-64 checksums, record encode/decode | zstd, blake3, xxhash-rust |
| `Compactor` | Rewrites sealed packs that are mostly garbage | `Index`, `PackReader`, `PackWriter` |
| `ReadTracker` | Read generation tracking, for safely deleting retired packs | none |

The `Index` trait (`get_segment`, `get_block`, `lookup_dedup`, `commit_batch`, and so on) exists
so redb can be swapped for LMDB or a custom index without touching the pack, codec, or GC code.

### On-disk layout

```
store/
  LOCK                 exclusive lock file (fs4)
  index.redb           metadata
  packs/
    00000001.pack      append-only, rotated at max_pack_size
    00000002.pack
```

### Pack record format

Each record is self-describing. The header is 40 bytes, little-endian:

| Field | Type | Notes |
|---|---|---|
| magic | u32 | record marker |
| flags | u8 | bit 0: compressed (1) or raw (0) |
| reserved | [u8; 3] | zero |
| raw_len | u32 | uncompressed length (<= block_size) |
| stored_len | u32 | length of payload |
| hash | [u8; 16] | BLAKE3 of the uncompressed data, truncated to 128 bits |
| checksum | u64 | xxh3-64 of the stored payload bytes |
| payload | [u8; stored_len] | zstd frame, or raw bytes |

A block is stored raw when zstd output is not smaller than the input.

### Hashing

- **Dedup key: BLAKE3 truncated to 128 bits.** Collision-resistant even against deliberately
  crafted input (the store may hold third-party content). Used only on the write path; costs
  about 15-25 µs per 64 KiB block, against about 400-600 µs for zstd-6.
- **Read integrity: xxh3-64 over the stored bytes.** Checked on every read, about 2 µs.
  The read path never computes BLAKE3.
- xxh3-64 as the dedup key was rejected: at 16M blocks the chance of an accidental collision is
  about 1 in 130,000 per dataset. xxh3-128 was rejected because it is not collision-resistant
  against hostile input, and byte-verifying on every dedup hit costs more than BLAKE3 does.

### redb tables

| Table | Key | Value |
|---|---|---|
| `files` | `file_id` ++ `segment: u32` (big-endian) | Segment 0 starts with a header (`len: u64`, `flags: u32`). Every segment holds up to 4096 `u64` block ids; 0 means not cached. |
| `blocks` | `block_id: u64` | `pack: u32, offset: u64, stored_len: u32, raw_len: u32, refcount: u32` (24 bytes) |
| `dedup` | `hash: [u8; 16]` | `block_id: u64` |
| `packs` | `pack_id: u32` | `live_bytes: u64, committed_len: u64, state: u8` (active / sealed / retired) |
| `meta` | `&str` | `schema_version`, `block_size` |

- **Segments** of 4096 blocks cover 256 MiB of file data. Files up to 256 MiB, which is nearly all
  of them, open with one lookup. Editing a very large file rewrites a 32 KiB segment, not the
  whole manifest.
- **`block_id` indirection** lets compaction move a block by updating one `blocks` row, never
  the manifests that reference it. Ids are `u64`, allocated monotonically, never reused.
- **Garbage in a pack** = pack file length - `live_bytes`. Records that were appended but never
  committed are counted as garbage automatically.
- Values are fixed-layout little-endian structs read in place with `zerocopy` (unaligned types),
  with no serialization step.

### Size estimate at 1 TB (16M blocks)

- `blocks`: about 500 MB
- `dedup`: about 390 MB
- manifests: about 130 MB
- total index on disk: roughly 1-1.5 GB including B-tree overhead
- resident memory: the redb cache size (default 64 MiB) plus small per-thread buffers

## 3. Public API

```rust
let store = BlockStore::open(path, StoreConfig::default())?;

store.set_len(file_id, len)?;                        // creates the file if missing
store.write_blocks(file_id, first_block, &data)?;    // block-aligned writes
store.read(file_id, offset, &mut buf)?;              // -> ReadResult { bytes: usize, missing: Vec<Range<u64>> }
store.stat(file_id)?;                                // -> Option<FileInfo { len }>
store.cached_ranges(file_id)?;                       // -> Vec<Range<u64>>
store.delete(file_id)?;
store.flush()?;
store.compact(CompactOptions { min_garbage_ratio: 0.5, max_bytes })?;
store.stats()?;                                      // per-pack size, live bytes, garbage ratio; healed-block count
store.verify()?;                                     // full consistency check (fsck)
```

`BlockStore` is `Send + Sync`. Any number of threads may read concurrently. Writes are
serialized only at the index commit; hashing and compression run in parallel with no lock held.

### Semantics

- `set_len` on a missing file creates it with every block missing. Shrinking drops the blocks
  past the new end of file. If the new length cuts through a block, that block is dropped
  (becomes missing), because its stored length no longer matches. Growing a file whose last
  block was short drops that block too, for the same reason.
- `write_blocks` requires the file to exist (`NotFound` otherwise). `first_block` must be within
  the file (`OutOfRange`). `data` must contain whole blocks, except that the file's final block
  must be exactly `len - idx * block_size` bytes (`Unaligned` otherwise).
- `read` fills `buf` with the cached bytes of the requested range, clamped to end of file, and
  returns the missing byte ranges at block granularity. Bytes in `buf` for missing ranges are
  left unspecified.
- Concurrent writes to the same file: last commit wins, per block slot.

## 4. Write path

`write_blocks(file_id, first_block, data)`:

1. **Validate** alignment, range and file existence.
2. **Chunk** the data into chunks of `write_txn_bytes` (default 16 MiB, 256 blocks). Each chunk
   is one transaction. If a later chunk fails, earlier chunks stay committed, and the error
   (`PartialWrite { blocks_written, source }`) reports how far the write got.
3. **Per block, in parallel (rayon), with no locks held:**
   - Compute the BLAKE3-128 hash.
   - Look up `dedup` in a read transaction. On a hit, use that `block_id` and skip compression.
   - On a miss, compress with zstd at the configured level, fall back to raw if that is not
     smaller, and compute xxh3-64 of the stored bytes.
   - Identical blocks within one chunk are compressed only once.
4. **Append** new records through `PackWriter` (mutex plus buffered writer). If the next record
   would exceed `max_pack_size`, the active pack is sealed, a new one is created, and the new
   pack is registered in the pack set before any index entry references it.
5. **Commit** one redb write transaction:
   - Re-check `dedup` for each new hash. If another thread inserted it meanwhile, use that id;
     the record just appended becomes garbage.
   - Re-check each dedup-hit `block_id` from step 3. If it was freed meanwhile (refcount reached
     zero in another commit), abort the transaction, compress and append those blocks, and retry
     the chunk. This is rare: it needs a concurrent delete of the only other reference.
   - Insert new `blocks` and `dedup` rows. Increment the refcount for each reference.
   - For each overwritten slot, decrement the old block's refcount. At zero, delete its
     `blocks` and `dedup` rows and subtract its record size from the pack's `live_bytes`.
   - Write the updated manifest segment(s).
   - Commit with `Durability::None`.

## 5. Flush

`flush()`:

1. Flush the `PackWriter` buffer and `sync_data` the active pack.
2. Record the active pack's synced length in `packs.committed_len`.
3. Commit with `Durability::Immediate`. This makes every earlier `Durability::None` commit durable.

Packs are always synced before the index becomes durable, so a durable index never points at
bytes that are not on disk.

## 6. Read path

`read(file_id, offset, buf)`:

1. Enter the `ReadTracker` (records the current read generation).
2. Open a redb read transaction (a snapshot).
3. Look up the manifest segment(s) covering the range. Clamp the range to end of file.
4. For each block: id 0, or an id with no `blocks` row, goes into `missing`. Otherwise look up
   its location.
5. Positioned read (`FileExt::read_at` on Unix, `seek_read` on Windows; no shared seek position)
   of header plus payload into a thread-local buffer. Check magic, lengths and xxh3-64.
6. Decompress straight into `buf` when the whole block is requested, otherwise through a
   thread-local scratch buffer.
7. Leave the `ReadTracker`.

Pack file handles stay open for the life of the store (at most about 256 at 1 TB with 4 GiB packs).

Expected cost of a warm 64 KiB block read: about 2 index lookups (~2 µs), one checksum (~2 µs),
one zstd decompression (~50-70 µs).

## 7. Delete, GC and compaction

### Delete

`delete(file_id)` runs one write transaction. It walks the file's segments, decrements the
refcount of each block id (freeing `blocks` and `dedup` rows at zero and updating
`live_bytes`), then removes the segments. Cost is proportional to the file's block count.
Decrementing an id with no `blocks` row (a healed block) is a no-op.

### Compaction

Runs only when the application calls `compact(opts)`. `max_bytes` bounds the I/O per call.

1. **Candidates:** sealed packs with garbage ratio >= `min_garbage_ratio`, worst first. The
   active pack is never compacted.
2. **Scan** the pack sequentially. For each record, `dedup[hash]` gives the `block_id` and
   `blocks[block_id]` gives its location. If the location is this pack and offset, the record
   is live, and its bytes are copied unchanged (no recompression) to the active pack.
   Otherwise it is skipped.
3. **Commit in batches.** Each batch transaction repoints `blocks` rows, but only rows still
   pointing at the old location (a concurrent overwrite may have freed the block). It moves
   `live_bytes` from the old pack to the destination.
4. **Retire.** After all live records have moved: `sync_data` the destination, set the old
   pack's state to `retired`, and commit with `Durability::Immediate`. Increment the read
   generation.
5. **Delete** the retired pack's file once every read that started before its retirement
   generation has finished (`ReadTracker`). Close the handle first. If deletion fails (for
   example an antivirus scanner on Windows holds the file), the pack stays `retired` and
   deletion is retried on the next `compact()` or `open()`.

Compaction temporarily needs free disk space equal to one pack's live bytes (at most `max_pack_size`).

## 8. Open and crash recovery

`BlockStore::open`:

1. Take an exclusive lock on `LOCK` (fs4). If it is held, return `Locked`.
2. Open (or create) `index.redb`. After an unclean shutdown redb rolls back to the last durable
   commit, possibly after a repair pass. Check `meta.schema_version` and `meta.block_size`
   against the config.
3. Reconcile packs:
   - The active pack is truncated to its `committed_len`. Bytes after it were never referenced
     by a durable index, and truncating keeps packs scannable for compaction. Appending then
     resumes on it.
   - Pack files on disk that are not in `packs` are deleted (created after the last durable commit).
   - Packs in state `retired` are deleted.

**Guarantee:** everything written before the last successful `flush()` is readable after a
crash, and no read ever returns bytes other than what was written to that block.

## 9. Error handling

```rust
enum Error {
    Io(std::io::Error),
    Index(redb::Error),              // wrapped redb errors
    NotFound,                        // file id not in the store
    Unaligned,
    OutOfRange,
    FileIdTooLong,
    Locked,                          // another process holds the store
    Corrupt { pack: u32, offset: u64, reason: &'static str },
    PartialWrite { blocks_written: u64, source: Box<Error> },
}
```

**Self-healing corruption.** If a record fails its magic, length or checksum check on read:

1. Log it with `tracing`.
2. In one transaction, delete the block's `blocks` and `dedup` rows.
3. Report the block's range as `missing`.

Other manifests referencing that `block_id` then also see it as missing. `stats()` counts
healed blocks. `Corrupt` is returned only by `verify()` and compaction, not by `read`.

## 10. Configuration

```rust
StoreConfig {
    block_size: 64 * 1024,          // fixed at store creation, saved in `meta`
    zstd_level: 6,                  // may change between opens; applies to new writes only
    max_pack_size: 4 << 30,
    index_cache_bytes: 64 << 20,
    write_txn_bytes: 16 << 20,
    max_file_id_len: 256,
    compression_threads: None,      // None = rayon default
}
```

## 11. Testing

- **Unit tests:**
  - Codec round-trips, including the raw fallback.
  - Record encode and decode.
  - Rejection of corrupted headers and payloads.
  - Manifest segment encoding.
  - Alignment validation.
  - `set_len` edge cases.
- **Model-based property tests (proptest):** random sequences of `set_len`, `write_blocks`,
  overwrite, `delete`, `compact`, `flush` and close-then-reopen, run against an in-memory model.
  After each step:
  - Reads and `cached_ranges` match the model.
  - Refcounts equal the actual number of references.
  - Every pack's `live_bytes` equals the sum of its live records.
- **Crash tests:** failpoints (`fail` crate) in the pack writer, the index commit and each
  compaction step. After a simulated crash and reopen:
  - Everything flushed is present.
  - No read returns wrong bytes.
  - `verify()` passes.
- **CI:** Windows, macOS and Linux.
- **Benchmarks (criterion):**
  - `stat` over 500k files.
  - Warm random 64 KiB reads.
  - Write throughput at zstd-6, with and without dedup.
  - Compaction throughput.
- **Scale test** (`#[ignore]`): about 100 GB of synthetic data; checks index size and resident
  memory against the section 2 estimates.

## 12. Dependencies

Runtime: `redb`, `zstd`, `blake3`, `xxhash-rust` (xxh3), `rayon`, `zerocopy`, `fs4`,
`thiserror`, `tracing`.
Dev: `proptest`, `tempfile`, `criterion`, `fail`.

## 13. Verify before implementation

These assumptions about redb 4.x must be confirmed against its docs and changelog:

1. `Durability::None` commits are visible to later read transactions, are rolled back on crash,
   and become durable at the next `Durability::Immediate` commit.
2. There are no file format or API changes in 4.x that affect this design (for example
   durability variants, cache configuration, zero-copy `AccessGuard` values).
3. Read transactions are cheap enough to open once per `read` call.
4. Behavior and performance of the repair pass on a roughly 1.5 GB database after an unclean shutdown.
