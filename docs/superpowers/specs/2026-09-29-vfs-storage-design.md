# `vfs-storage`: the block store as cache and layer storage — design

**Status:** approved in conversation 2026-09-29; this document is the written
form for review.

## 1. Goal

`vfs-block-store` replaces `vfs-cache`, in two roles:

1. **Pull-through cache** for read-only sources: blocks fetched from a slow
   source are kept, deduplicated and compressed, and served locally next time.
2. **Storage for mutable file data**: a session's write layer — saves, edited
   INIs, anything the game creates — lives in the store as a **named layer**
   instead of a directory of plain files.

Both share one store per `VFS_HOME`, so a copied-up game file costs almost
nothing: its blocks are dedup hits on blocks the cache already holds.

**Done means:** the daemon runs on `vfs-storage`; `vfs-cache` is gone; a
session whose write layer is `layer:NAME` keeps a Proton game's writes across
`vfs down` and a daemon restart and can export them; a slow source's reads are
served from the store on the second pass; CI proves both.

## 2. Where things stand

Measured 2026-09-29:

- `vfs-cache` is a **RAM-only** cache in production: the daemon builds
  `BlockCache` with `CacheConfig::default()` (1 MiB blocks, 64 MiB, no disk
  tier). The `[cache]` config block is parsed and never read.
- Its keys are weak: `file_id` hashes root, path, size and mtime, but
  `DiskProvider` reports `mtime: 0`; and `source_id` restarts at 1 in every
  session while the cache is daemon-wide, so **two sessions can serve each
  other's cached blocks**.
- It wraps **every** source, including local disk sources that the OS page
  cache already serves.
- No source supplies a content hash: zip CRC32s are parsed and discarded, and
  the remote protocol's `OpenResp.file_id` is always 0.
- Mutable data lives in a `DiskProvider` directory under an `OverlayProvider`
  (copy-up, `.wh.` whiteouts, rename-by-copy-up).
- `vfs-block-store` stores bytes and a length per caller-chosen id. It has **no
  eviction, no enumeration, no rename, whole-block writes only**, reads a hole
  as "missing" rather than zeros, allows one process per store directory, and
  has no decompressed-block cache (a 64 KiB read decompresses in ~50 µs, where
  a `vfs-cache` RAM hit is ~0.1 µs).

## 3. Shape

A new crate, **`vfs-storage`**, depends on `vfs-block-store` and
`vfs-provider`:

```
Storage (one per process, one per directory)
  ├── BlockStore          <dir>/            (packs + index.redb)
  ├── Catalog (redb)      <dir>/catalog.redb
  └── RamTier             decompressed blocks, sharded CLOCK
        ▲                          ▲
  CachedSource (Provider)    LayerProvider (Provider)
  wraps an immutable+slow    one named, persistent,
  source; pull-through       read-write namespace
```

- `Storage::open(dir, StorageConfig) -> Result<Arc<Storage>>`. `StorageConfig`
  carries the block store's config plus `cache_max_bytes` (default **32 GiB**)
  and `ram_tier_bytes` (default **256 MiB**).
- `Storage::cached(source: Arc<dyn Provider>, key: SourceKey) -> Arc<dyn Provider>`
  and `Storage::layer(name: &str) -> Result<Arc<dyn Provider>>`.
- `Storage::layers() -> Vec<LayerInfo>`, `export_layer(name, dir)`,
  `import_layer(dir, name)`, `delete_layer(name)`, `stats()`, `close()`.

**Who owns it:**

- The **daemon** opens `Storage` once at startup at `$VFS_HOME/storage`
  (overridable with `vfs daemon --storage-dir DIR` or `VFS_STORAGE_DIR`; cache budget with
  `--cache-max-gib N`). It is process-wide because the store allows one
  process per directory.
- `SessionRegistry` takes an `Option<Arc<Storage>>`. Without one, sources are
  served uncached and a `layer` source is refused by name — which is how the
  existing in-process daemon tests keep running unchanged, and how they get a
  store when they want one (a temp directory).
- **Embedded hosts** opt in by opening their own `Storage` and wrapping
  providers with it. `Session` does not own one.
- `vfs-embed` re-exports `Storage`, `StorageConfig`, `SourceKey`; its
  `BlockCache`/`CachingProvider`/`CacheConfig` re-exports are removed.
- **`vfs-cache` is deleted**, with its CI entry, README rows and docs updated.
  Its sharded CLOCK RAM store is **ported** into `vfs-storage` as the RAM tier
  (its tests come with it), not rewritten.
- The per-session `[cache]` config block is **removed**: a config that still
  has one loads with a one-line warning naming the daemon flags, not an error.
- The daemon's `Stats` RPC reports `Storage::stats()`. `StatsResp`'s existing
  fields keep their numbers: `cache_hits` = RAM-tier + store hits,
  `cache_misses` = blocks fetched from a source, `cache_evicts` = RAM-tier
  evictions, `cache_disk_hits` = store hits, `cache_bytes_from_cache` /
  `cache_bytes_from_source` as named, `cache_ram_bytes` = RAM-tier bytes. New
  fields: pack bytes, live bytes, cached logical bytes, layer count.

`vfs-block-store` gains exactly one API: **`BlockStore::file_ids()`**,
iterating every stored file id, which reconciliation (§6) needs. Nothing else
in that crate changes.

## 4. Pull-through cache: `CachedSource`

**Which sources are cached.** Only sources whose capabilities declare
`immutable && slow` — today, remote/gRPC sources; later, CDN/HTTP. This is the
rule `vfs-provider`'s README and `caps.rs` already state. Local disk and zip
sources are **not** wrapped: the OS page cache serves them, compressing them
into the store costs more than it saves, and a disk source the user edits
(a mod folder) would go stale. `Storage::cached` on any other source returns
it unchanged, so a caller cannot get this wrong.

**File id** = BLAKE3 truncated to 128 bits of, length-prefixed:

1. the **source key** — `SourceKey`, stable across runs: for a remote source
   its endpoint; a config may set `cache_key = "..."` on any `[[source]]` to
   pin it (e.g. a CDN whose endpoint rotates);
2. the file's normalized path;
3. its size;
4. a **version tag**: the remote protocol's `OpenResp.file_id` when non-zero,
   else the `Stat.mtime`.

Different content can never share an id unless path, size and version all
match within one source key. Identical content from different sources still
deduplicates — at block level, inside the store.

**Read path.** RAM tier → `BlockStore::read` → for each missing range, fetch
whole blocks from the source (block-aligned; a file's final block exactly its
remaining length), `write_blocks` them, fill the buffer. The store file and its catalog
row are created when the file's first block is stored, not at open, so a
file that is opened but never read leaves nothing behind. Concurrent misses on the same
`(file id, block)` are coalesced into one fetch.

**Eviction.** The catalog's `cache_files` table records, per cached file id,
its last-access minute and its cached **logical** bytes. Access times are
batched in memory and committed at most once a minute. After a write that
takes cached logical bytes over `cache_max_bytes`, least-recently-used files
are `delete`d until under 90% of the budget, then `compact` runs with its
default options on a background thread. Layer data is never counted and never
evicted. (Logical bytes overstate disk use, because of dedup and compression;
they are predictable, which matters more for a budget.)

## 5. Layers: `LayerProvider`

A **layer** is a named, persistent namespace in the catalog. `layer_entries`
maps `(layer, folded path)` to `{ name (original case), kind (file|dir),
guid (16 bytes), len, mtime }`. Lookup is case-insensitive
(`CaseMatch::Insensitive`), matching Windows.

It is a full `Access::ReadWrite`, `immutable: false` provider and plugs in
exactly where a `DiskProvider` write layer goes today — the upper of
`OverlayProvider` via `compose_root`. Copy-up, whiteouts (`.wh.` markers are
just small files in the layer) and rename-by-copy-up are unchanged.

- **Unaligned writes.** Each writable handle keeps a small dirty-block buffer:
  a write that covers part of a block reads that block (from the handle's
  buffer, the RAM tier or the store), patches it, and holds it; sequential
  writes coalesce; whole blocks are committed with `write_blocks` when the
  buffer passes 4 blocks, on `flush`, and on `close`.
- **Holes and length.** Extending a file (by `set_len` or a write past the
  end) writes explicit zero blocks — all dedup hits on one block — so a layer
  file never has a "missing" block. `set_len` rewrites the tail block.
- **Rename** changes catalog rows only; the GUID moves, no data is copied.
  **Remove** deletes the catalog row, then the GUID in the store.
- **Durability.** `Provider::flush`, and `close` of a handle that wrote,
  commit the handle's dirty blocks (non-durably). A *durable point* is
  `BlockStore::flush()`, then the catalog's durable commit.
  `StorageConfig::durability` picks when one runs (amended 2026-09-30, after
  per-close fsyncs measured ~25 ms per closed file on btrfs with no parallel
  speedup):
  - `Durability::Deferred { max_interval }`, **the default** (5 minutes):
    closes, flushes and namespace changes make no durable point unless the
    last one is `max_interval` old; then the operation runs one for every
    live layer. `Storage::sync()`, `Storage::close()`, a layer provider's
    drop, and layer create/import/delete always run one. A crash loses the
    changes since the last durable point: files created since are gone
    whole, removals and renames are undone (a removed file's data is
    deleted only after a durable point), and a file rewritten in place may
    come back old, new or mixed. Game-save durability is a later concern.
  - `Durability::OnEveryClose`: the original rule — flush, the close of a
    handle that wrote, and every namespace change run a durable point before
    returning; a crash loses only writes on handles still open.
- **Corruption.** A missing block in a layer file cannot be refetched, so it is
  corruption: the read returns `ST_IO_ERROR` and the event is logged with the
  layer, path and block. It is never served as zeros.
- The RAM tier entries of a GUID are dropped when that GUID's blocks are
  written.

## 6. Catalog ↔ store consistency

The catalog and the block store commit separately, so ordering and
reconciliation carry consistency:

- **Create:** a file's GUID is written to the catalog (non-durably) before the
  store `set_len`s it.
- **Flush:** `BlockStore::flush()` completes before the catalog's durable
  commit, so every durable catalog row references durable store data.
- **Delete:** catalog row first, durably (a durable point), store delete
  second. Under deferred durability the store delete waits for the next
  durable point.
- **At `Storage::open`, reconcile:**
  - a catalog file whose GUID the store lacks → recreated as an empty file,
    logged (its data was lost in a crash before a flush);
  - a store id no catalog row references (layer file or cache file) →
    deleted (a create or delete that never completed);
  - then `compact` if anything was deleted.

## 7. Configuration and CLI

- **Config:** `[[source]] type = "layer"` with `name = "skyrim-profile-a"`,
  valid only with `write_layer = true`. `cache_key` is an optional field on
  any `[[source]]`. `vfs-control`'s `SourceSpec` gains `Layer { name }`;
  `director.proto`'s `SourceSpec` gains `LayerSource { name }` (additive).
- **CLI:** `vfs launch --write-layer layer:NAME`; the existing
  `--write-layer DIR` keeps meaning a disk directory.
- **Layer commands:** `vfs layer list`, `vfs layer export NAME DIR`,
  `vfs layer import DIR NAME`, `vfs layer delete NAME` — daemon RPCs, since
  the daemon holds the store. `export` writes plain files (and no `.wh.`
  markers); `import` refuses an existing layer name; `delete` refuses a layer
  a live session uses.
- **Disk write layers stay supported** exactly as today: a `DiskProvider` is
  still a provider. Existing e2e tests and `skyrim-live` keep using them;
  nothing new is built for them.

## 8. Verification

| gate | proves | where |
|---|---|---|
| `LayerProvider` passes `assert_conformance` (incl. `assert_writable`) | it is a correct read-write provider | unit, both OSes |
| `CachedSource` passes `assert_conformance` over the existing fixtures | caching changes no answer | unit, both OSes |
| unit: unaligned writes, holes, truncate/extend, rename, remove, flush-on-close durability (reopen `Storage`), corruption → `ST_IO_ERROR` | layer semantics | unit |
| unit: reconciliation after simulated crashes (catalog-only row, store-only id) | §6 | unit |
| unit: LRU order, budget enforcement, layers never evicted, coalesced concurrent misses | §4 | unit |
| RAM tier: the ported `vfs-cache` store tests | tier correctness | unit |
| **Linux e2e**: `proton_cli` variant with `write_layer` = `layer:e2e-<pid>`; the fixture's write is present after `vfs down`, after a daemon restart, and in `vfs layer export` | the headline workflow | CI `proton-linux` |
| daemon test: a remote (gRPC `vfs-source`) source read twice; `Stats` shows hits and bytes from cache on the second pass | pull-through end to end | CI, both OSes |
| existing suites | no regression (disk write layers, overlay, e2e) | CI |

## 9. What must not regress

- **Disk write layers, `OverlayProvider`, and every existing e2e test** behave
  as today.
- **No ring/shim change.** Storage lives entirely in the Director's provider
  graph; `bin/regen-protocol` stays clean.
- **gRPC:** additive only (`LayerSource`, layer RPCs, new `StatsResp` fields).
- **No unregistered env switch.** One is added and registered in `vfs_env`:
  `VFS_STORAGE_DIR`, the daemon's storage directory (same as
  `--storage-dir`). An auto-spawned daemon takes no flags, and a test daemon
  must not contend for the lock on the user's real `$VFS_HOME/storage`.
- **Windows:** `vfs-storage` is portable; the Windows CI job runs its tests.

## 10. Out of scope

- Caching local disk or zip sources.
- Prefetch/readahead into the cache.
- Migrating existing disk write-layer directories into layers (`vfs layer
  import` covers the manual path).
- Multi-process access to one store.
- Content hashes computed from source bytes before a read.
