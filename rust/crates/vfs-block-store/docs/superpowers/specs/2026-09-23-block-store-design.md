# Block Store Design

Date: 2026-09-23
Status: Approved. Amended during implementation planning; section 14 lists the amendments.

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
| `Index` (module `index`) | All metadata: manifests, block locations, dedup, pack accounting. The only module that touches redb. | redb |
| `PackWriter` | Owns the single active pack: appends records, rotates at `max_pack_size` | `Codec` |
| `PackReader` | Positioned reads of records by (pack, offset); keeps pack handles open | `Codec` |
| `Codec` | zstd compress/decompress, BLAKE3-128 hashing, xxh3-64 checksums, record encode/decode | zstd, blake3, xxhash-rust |
| `Compactor` | Rewrites sealed packs that are mostly garbage | `Index`, `PackReader`, `PackWriter` |
| `ReadTracker` | Read generation tracking, for safely deleting retired packs | none |

No redb type appears outside the `index` module. Its interface (`read()` snapshots,
`update(durable, |tables| ...)` transactions, and typed getters and setters) is the seam for
swapping redb for LMDB or a custom index later. Extracting a formal trait at that point is
mechanical; with one implementation it would be premature.

### On-disk layout

```
store/
  LOCK                 exclusive lock file (std File::try_lock)
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
| `files` | `(file_id: &[u8], segment: u32)` (redb tuple key) | Segment 0 starts with a 16-byte header (`len: u64`, 8 reserved bytes). Every segment holds up to 4096 `u64` block ids; 0 means not cached. |
| `blocks` | `block_id: u64` | `pack: u32, offset: u64, stored_len: u32, raw_len: u32, refcount: u32, hash: [u8; 16]` (40 bytes) |
| `dedup` | `hash: [u8; 16]` | `block_id: u64` |
| `packs` | `pack_id: u32` | `live_bytes: u64, state: u8` (active / sealed / retired) |
| `meta` | `&str` | `schema_version`, `block_size`, `next_block_id`, `next_pack_id`, `clean_shutdown` |

- **The block's hash is stored in its `blocks` row** so that freeing a block can delete its
  `dedup` row without reading the pack. Invariant: a `dedup` row exists exactly when a `blocks`
  row with that hash exists.

- **Segments** of 4096 blocks cover 256 MiB of file data. Files up to 256 MiB, which is nearly all
  of them, open with one lookup. Editing a very large file rewrites a 32 KiB segment, not the
  whole manifest.
- **`block_id` indirection** lets compaction move a block by updating one `blocks` row, never
  the manifests that reference it. Ids are `u64`, allocated monotonically, never reused.
- **Garbage in a pack** = pack file length - `live_bytes`. Records that were appended but never
  committed are counted as garbage automatically.
- Values are fixed-layout little-endian byte arrays, decoded field by field with
  `from_le_bytes` straight from redb's zero-copy value slices. There is no serialization library.
- Read snapshots open tables lazily, so `stat` opens only `files`.

### Size at 1 TB (16M blocks)

Measured with the scale test (16-32 GiB runs, extrapolated):

- keys and values: about 76 bytes per block (`blocks` 48, `dedup` 24, manifests about 8)
- B-tree pages in use: about 103 bytes per block, so about 1.65 GB at 16M blocks
- the index file grows in large steps, so it can be somewhat larger than the pages in use
- resident memory: about 35 MiB baseline plus the redb cache (default 64 MiB); it plateaus at
  the cache size (measured 48-81 MiB peak with 16-64 MiB caches)

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
store.index_size()?;                                 // index pages in use and bytes stored
store.verify()?;                                     // full consistency check (fsck)
store.close()?;                                      // flush and mark a clean shutdown (Drop does the same)
```

`compact` returns a `CompactReport { packs_compacted, bytes_read, bytes_moved }`.

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
   would exceed `max_pack_size`, the active pack is fsynced and sealed, and a new pack is
   registered in the index (a separate `Durability::None` commit) **before** its file is created.
   A crash between the two leaves only an orphan row, which open removes. The buffer is flushed
   to the OS before the commit, so readers can see the bytes.
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

`flush()`, holding the pack writer lock throughout:

1. Flush the `PackWriter` buffer and `sync_data` the active pack. Sealed packs were synced when
   they were sealed.
2. Commit with `Durability::Immediate`. This makes every earlier `Durability::None` commit durable.

Packs are always synced before the index becomes durable. Holding the writer lock means nothing
can be appended between the sync and the durable commit, so a durable index never points at
bytes that are not on disk.

**Automatic flush.** redb keeps memory for non-durable commits until the next durable commit
(redb 3.0 changelog). So after `auto_flush_bytes` (default 1 GiB) of appended records, the write
path calls `flush()` itself.

`close()` (and `Drop`) flushes and also sets `meta.clean_shutdown = 1` in the same durable commit.

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
   Otherwise it is skipped. A live record whose checksum fails is healed instead of copied.
   The scan stops at the first undecodable header, which is the torn tail of a pack sealed after
   a crash.
   **Evacuate by index:** afterwards, any block the index still places in this pack (behind a
   corrupt header, for example) is found by scanning `blocks` and moved the same way. Normally
   there are none.
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

1. Take an exclusive lock on `LOCK` (`File::try_lock`). If it is held, return `Locked`.
2. Open (or create) `index.redb`. After an unclean shutdown redb rolls back to the last durable
   commit (verified: reopening after a real process abort took about 3 ms on a small database).
   Check `meta.schema_version` and `meta.block_size` against the config.
3. In one durable transaction:
   - Read `meta.clean_shutdown`, then set it to 0.
   - Delete the files of packs in state `retired` and remove their rows. A file that is already
     gone counts as deleted.
   - Remove rows of packs whose file is missing and whose `live_bytes` is 0 (registered, never
     created). A missing file with live data is `Corrupt`.
   - Delete pack files on disk that have no row. They were created after the last durable
     commit and hold nothing referenced.
   - **After a clean shutdown**, resume appending to the newest `active` pack; its file ends
     exactly at the last record. **After a crash**, seal every `active` pack instead, and the
     next write starts a new pack. The sealed pack's tail may be torn, but no durable index
     entry points into the torn part, and compaction stops scanning there.

**Guarantee:** everything written before the last successful `flush()` is readable after a
crash, and no read ever returns bytes other than what was written to that block.

## 9. Error handling

```rust
enum Error {
    Io(std::io::Error),
    Index(Box<dyn std::error::Error + Send + Sync>), // any redb error, boxed
    NotFound,                        // file id not in the store
    Unaligned,
    OutOfRange,
    FileIdTooLong,
    Locked,                          // another process holds the store
    Corrupt(String),                 // index inconsistency (for example a missing manifest segment)
    Config(String),                  // invalid StoreConfig, or block_size differs from the store's
    PartialWrite { blocks_written: u64, source: Box<Error> },
}
```

**Self-healing corruption.** If a record fails its magic, length or checksum check on read:

1. Log it with `tracing`.
2. In one transaction, delete the block's `blocks` and `dedup` rows.
3. Report the block's range as `missing`.

Other manifests referencing that `block_id` then also see it as missing. `stats()` counts
healed blocks. Corrupt records never produce an error from `read`; `verify()` reports them in
`VerifyReport::problems`, and compaction heals them.

## 10. Configuration

```rust
StoreConfig {
    block_size: 64 * 1024,          // fixed at store creation, saved in `meta`
    zstd_level: 6,                  // may change between opens; applies to new writes only
    max_pack_size: 4 << 30,
    index_cache_bytes: 64 << 20,
    write_txn_bytes: 16 << 20,
    auto_flush_bytes: 1 << 30,      // durable flush after this many appended bytes (bounds redb memory)
    max_file_id_len: 256,
    compression_threads: None,      // None = rayon default
}
```

`open` rejects out-of-range values with `Config`: `block_size` must be 4 KiB-16 MiB, `zstd_level`
must be in zstd's range, `max_pack_size` must be at least two blocks, and `max_file_id_len` must be 1-4096.

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
- **Crash tests:** the test binary re-runs itself as a child process, and the child aborts at a
  named crash point (`crash::point`, compiled in only with the `crash-points` feature): after
  append and before commit, in flush before the durable commit, and after copy, before retire
  and after retire during compaction. After the real crash and reopen:
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

Runtime: `redb` 4.3, `zstd` 0.14, `blake3` 1.8, `xxhash-rust` 0.8 (xxh3), `rayon` 1.12,
`thiserror` 2, `tracing` 0.1.
Dev: `proptest`, `tempfile`, `criterion`, `memory-stats`.
MSRV: Rust 1.89 (`File::try_lock`).

## 13. Verification results (redb 4.3)

1. **Confirmed** by a real process abort on Windows: `Durability::None` commits are visible to
   later reads, are rolled back on crash, and become durable at the next `Immediate` commit.
2. **Confirmed**, with one consequence: since 3.0, `Durability` has only `None` and `Immediate`,
   and non-durable commits hold RAM until the next durable commit. That is why the store flushes
   automatically after `auto_flush_bytes` (section 5). The 4.0 change (drop `AccessGuardMut`
   before its table) does not affect this design, which never uses it.
3. **Confirmed:** a `stat` (one snapshot plus one lookup) takes about 1.3 µs, so 500,000 files
   take about 0.65 s. A warm 64 KiB read takes about 46-50 µs, dominated by decompression.
4. Repair after an unclean shutdown was fast in the crash tests (small databases). A 1.5 GB
   repair has not been measured; the scale test can be extended to do so.

## 14. Amendments made during implementation planning

Each was found while building and testing the design in full before writing the plan.

1. **`blocks` rows include the block's 16-byte hash** (40 bytes, not 24), so freeing a block can
   delete its dedup entry without reading the pack.
2. **No `committed_len`.** Recovery uses a clean-shutdown flag instead: resume the active pack
   only after a clean close, otherwise seal it and start a new one. Compaction stops scanning at
   a torn tail and evacuates any remaining live blocks by index scan.
3. **`auto_flush_bytes`** (default 1 GiB) bounds redb memory (section 13, item 2).
4. **New packs are registered in the index before their files are created.**
5. **`flush()` holds the pack writer lock** across the fsync and the durable commit.
6. **`Index` is a module boundary, not a trait** (section 2).
7. **Errors:** `Index` wraps any redb error in a box; `Corrupt(String)` covers index
   inconsistencies; `Config(String)` was added.
8. **Dependencies:** `fs4` was replaced by std `File::try_lock`, `fail` by a small feature-gated
   `crash::point`, and `zerocopy` by hand-written little-endian decoding.
9. **API additions:** `close()`, `index_size()`, and a `CompactReport` returned from `compact`.
10. **Read snapshots open tables lazily**, which made the 500k-file `stat` scan 40% faster
    (1.09 s to 0.65 s).
11. **Recovery bug found by the crash tests and fixed:** `open` deleted a retired pack's file and
    then tried to delete it again as an unregistered file. All pack deletions now treat "already
    gone" as success.
