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
