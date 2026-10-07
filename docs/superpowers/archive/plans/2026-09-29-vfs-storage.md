# `vfs-storage` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `vfs-cache` with `vfs-block-store`, as a pull-through cache for immutable+slow sources and as the storage for named, persistent write layers.

**Architecture:** A new crate `vfs-storage` owns one `BlockStore`, a redb catalog beside it, and a RAM tier of decompressed blocks (ported from `vfs-cache`). It hands out two `Provider`s: `CachedSource` (pull-through, LRU-evicted under a logical-byte budget) and `LayerProvider` (a read-write namespace whose files are GUID-keyed in the store). The daemon opens one `Storage` per process; configs name layers as write layers; `vfs-cache` is deleted.

**Tech Stack:** Rust stable; `vfs-block-store` (redb 4, zstd, blake3); redb for the catalog; tonic/prost gRPC; clap.

**Spec:** `docs/superpowers/specs/2026-09-29-vfs-storage-design.md`

## Global Constraints

- **Disk write layers, `OverlayProvider`, and every existing test behave as today.** Windows cannot run here: every task touching code that compiles on Windows must pass `env XWIN_ACCEPT_LICENSE=1 "CFLAGS_x86_64-pc-windows-msvc=-Wno-error=implicit-function-declaration" cargo xwin clippy --target x86_64-pc-windows-msvc -p <crate> --all-targets -- -D warnings` (from `rust/`, after `source ~/.cargo/env`); the real Windows run is CI.
- **No ring/shim change.** `bin/regen-protocol` then `git diff --exit-code resources/` stays clean.
- **gRPC changes are additive only.**
- **Env switches:** exactly one new one, `VFS_STORAGE_DIR`, registered in `vfs_env` (constant + `ALL` row). The lint `no_crate_reads_a_switch_that_is_not_registered` enforces registration.
- **`vfs-block-store` gains exactly one public API, `BlockStore::file_ids()`.** Its license (`MIT OR Apache-2.0`) and edition are untouched. `vfs-storage` is `GPL-3.0-only`, edition 2021, like the rest of the workspace.
- **Store file ids:** a layer file is `b'L'` + its 16-byte GUID (17 bytes); a cache file is `b'C'` + the 16-byte BLAKE3 identity hash (17 bytes). The prefix is how reconciliation tells them apart.
- **Defaults:** `cache_max_bytes` 32 GiB, `ram_tier_bytes` 256 MiB, eviction target 90% of budget, access times committed at most once a minute, layer handles commit dirty blocks past 4 blocks, on `flush` and on `close`.
- `cargo clippy --all-targets -- -D warnings` clean for touched crates on Linux.
- Commit author: `git -c user.name="Tim Baldridge" -c user.email=tbaldridge@gmail.com commit …`; conventional subjects (`feat(storage): …`).

## Review Focus

1. **A game writes a few bytes into the middle of a large existing layer file, then reads across the edited block** — expects exactly its bytes, neighbours unchanged (Task 4 test `partial_block_write_preserves_neighbours`).
2. **A layer file is extended past a never-written gap (seek + write, or `set_len` grow)** — the gap reads as zeros, never as an error (Task 4 test `gap_reads_as_zeros`).
3. **Two handles open on the same layer file, one writes, the other reads** — the reader sees the write without a close (Task 4 test `second_handle_sees_uncommitted_writes`).
4. **A slow source's file changes version (new size or new `file_id`) between sessions** — the new content is served, never the old cached blocks (Task 3 test `version_change_is_a_new_cache_file`).
5. **The daemon is killed between a layer write's close and its next flush point, then restarted** — the closed file's bytes are there (Task 5 test `closed_file_survives_reopen_without_close`).

---

## Execution waves

| wave | tasks (parallel within a wave, one worktree each) |
|---|---|
| 1 | Task 1 (block-store `file_ids`), Task 2 (vfs-storage core: config, catalog, RAM tier), Task 6 (control plane: config/proto/env) |
| 2 | Task 3 (`CachedSource` + eviction), Task 4 (`LayerProvider`) |
| 3 | Task 5 (reconciliation + layer management) |
| 4 | Task 7 (daemon + CLI integration; delete `vfs-cache`) |
| 5 | Task 8 (e2e + CI + docs) |

---

### Task 1: `BlockStore::file_ids()`

**Files:** Modify `rust/crates/vfs-block-store/src/store.rs` (new method), `rust/crates/vfs-block-store/tests/lifecycle.rs` (test).

**Interfaces:** Produces `pub fn file_ids(&self) -> Result<Vec<Vec<u8>>>` — every stored file id once, in key order.

- [ ] **Step 1: Failing test** (append to `tests/lifecycle.rs`, using its existing helpers for opening a temp store):

```rust
#[test]
fn file_ids_lists_every_file_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = vfs_block_store::BlockStore::open(dir.path(), vfs_block_store::StoreConfig::default()).unwrap();
    // A file big enough to span two manifest segments must still be listed once.
    let big = 4096u64 * 64 * 1024 + 1;
    store.set_len(b"b-big", big).unwrap();
    store.set_len(b"a-small", 10).unwrap();
    store.set_len(b"c-gone", 10).unwrap();
    store.delete(b"c-gone").unwrap();
    assert_eq!(store.file_ids().unwrap(), vec![b"a-small".to_vec(), b"b-big".to_vec()]);
}
```

- [ ] **Step 2:** `cargo test -p vfs-block-store --test lifecycle file_ids` → compile error (no method).
- [ ] **Step 3: Implement** in `store.rs` beside `stat`:

```rust
    /// Every stored file id, once each, in key order. For callers that keep
    /// their own catalog and must reconcile it with the store after a crash.
    pub fn file_ids(&self) -> Result<Vec<Vec<u8>>> {
        let _guard = self.tracker.enter();
        let r = self.index.read()?;
        let mut out = Vec::new();
        r.for_each_segment(|id, seg, _| {
            if seg == 0 {
                out.push(id.to_vec());
            }
            Ok(())
        })?;
        Ok(out)
    }
```

Mirror `verify`'s use of `self.tracker.enter()` / `self.index.read()`; if either name differs, follow `verify.rs:48-52` exactly. Add one README line under the API section.
- [ ] **Step 4:** `cargo test -p vfs-block-store --all-features` → pass; `cargo clippy -p vfs-block-store --all-targets --all-features -- -D warnings` → clean.
- [ ] **Step 5: Commit** `feat(block-store): list stored file ids`

---

### Task 2: `vfs-storage` core — config, catalog, RAM tier, `Storage::open`

**Files:**
- Create: `rust/crates/vfs-storage/{Cargo.toml,src/lib.rs,src/config.rs,src/catalog.rs,src/ram.rs,src/storage.rs,src/ids.rs}`
- Modify: `rust/Cargo.toml` (member `crates/vfs-storage`, after `crates/vfs-block-store`)
- Source to port: `rust/crates/vfs-cache/src/store.rs` (the sharded CLOCK `BlockCache`) → `src/ram.rs`

**Interfaces (Produces):**
```rust
// config.rs
pub struct StorageConfig { pub store: vfs_block_store::StoreConfig, pub cache_max_bytes: u64, pub ram_tier_bytes: u64 }
impl Default for StorageConfig // store: StoreConfig::default(), cache_max_bytes: 32 << 30, ram_tier_bytes: 256 << 20

// ids.rs
pub type Guid = [u8; 16];
pub fn new_guid() -> Guid;                                  // uuid v4 bytes
pub fn layer_file_id(g: &Guid) -> [u8; 17];                  // b'L' + g
pub fn cache_file_id(h: &[u8; 16]) -> [u8; 17];              // b'C' + h
pub enum StoreIdKind { Layer(Guid), Cache([u8; 16]), Foreign }
pub fn classify_store_id(id: &[u8]) -> StoreIdKind;

// ram.rs — the ported BlockCache, re-keyed
pub struct RamTier { .. }
impl RamTier {
    pub fn new(budget_bytes: u64) -> Self;
    pub fn get(&self, file_id: &[u8; 17], block: u64) -> Option<std::sync::Arc<[u8]>>;
    pub fn put(&self, file_id: &[u8; 17], block: u64, data: std::sync::Arc<[u8]>);
    pub fn invalidate_file(&self, file_id: &[u8; 17]);
    pub fn stats(&self) -> RamStats; // hits, misses, evicts, bytes
}

// catalog.rs — redb at <dir>/catalog.redb
pub struct Catalog { .. }
pub struct EntryRec { pub name: String, pub kind: u8 /*KIND_FILE|KIND_DIR*/, pub guid: Guid, pub len: u64, pub mtime: i64 }
pub struct CacheRec { pub last_access_min: u64, pub logical_bytes: u64 }
impl Catalog {
    pub fn open(path: &std::path::Path) -> Result<Self, StorageError>;
    pub fn layer_id(&self, name: &str) -> Result<Option<u64>, StorageError>;
    pub fn create_layer(&self, name: &str) -> Result<u64, StorageError>;   // refuses an existing name
    pub fn layer_names(&self) -> Result<Vec<(String, u64)>, StorageError>;
    pub fn drop_layer(&self, layer: u64) -> Result<Vec<Guid>, StorageError>; // removes rows, returns their GUIDs
    pub fn get(&self, layer: u64, folded: &str) -> Result<Option<EntryRec>, StorageError>;
    pub fn put(&self, layer: u64, folded: &str, rec: &EntryRec, durable: bool) -> Result<(), StorageError>;
    pub fn remove(&self, layer: u64, folded: &str, durable: bool) -> Result<(), StorageError>;
    pub fn children(&self, layer: u64, folded_dir: &str) -> Result<Vec<EntryRec>, StorageError>; // direct children only
    pub fn rename(&self, layer: u64, from: &str, to: &str, to_name: &str) -> Result<(), StorageError>; // moves a subtree atomically
    pub fn all_layer_guids(&self) -> Result<Vec<(u64, String, Guid)>, StorageError>;
    pub fn cache_get(&self, id: &[u8; 16]) -> Result<Option<CacheRec>, StorageError>;
    pub fn cache_put_many(&self, recs: &[([u8; 16], CacheRec)]) -> Result<(), StorageError>;
    pub fn cache_remove(&self, id: &[u8; 16]) -> Result<(), StorageError>;
    pub fn cache_all(&self) -> Result<Vec<([u8; 16], CacheRec)>, StorageError>;
    pub fn commit_durable(&self) -> Result<(), StorageError>; // makes prior non-durable writes durable
}

// storage.rs
pub struct Storage { pub(crate) store: vfs_block_store::BlockStore, pub(crate) catalog: Catalog, pub(crate) ram: RamTier, pub(crate) cfg: StorageConfig, .. }
impl Storage {
    pub fn open(dir: impl AsRef<std::path::Path>, cfg: StorageConfig) -> Result<std::sync::Arc<Storage>, StorageError>;
    pub fn close(self: std::sync::Arc<Self>) -> Result<(), StorageError>; // flush store + catalog
    pub fn block_size(&self) -> u64;
}
pub enum StorageError { Store(vfs_block_store::Error), Catalog(String), Io(std::io::Error), LayerExists(String), NoSuchLayer(String), LayerInUse(String) }
impl StorageError { pub fn to_status(&self) -> i32 } // maps to vfs_provider ST_* (NotFound, Exists, Io)
```

Catalog keys: table `layers`: `&str name → u64 id` (+ `meta` counter for ids); table `entries`: `(u64 layer, &str folded_path) → &[u8] encoded EntryRec`; the root directory of a layer is the row with `folded_path = ""` (kind DIR), created by `create_layer`. `children` range-scans `(layer, "dir/")..` and keeps keys with no further `/` (for the root, keys with no `/` at all, excluding `""`). Folding uses `vfs_core::fold` (add `vfs-core` dependency). `rename` rewrites the moved key and every key under `from/` in one write transaction. `put`/`remove` with `durable: false` commit with `redb::Durability::None`; `commit_durable` runs an empty `Durability::Immediate` commit. Table `cache_files`: `[u8;16] → (u64, u64)`.

RAM tier port: copy `vfs-cache/src/store.rs` into `ram.rs`; change the key to `(file_id: [u8;17], block: u64)`; drop the disk tier (`disk_dir`, `.blk` files) and `source_id`; keep sharding, CLOCK, budget, oversized-reject, and **port the unit tests that apply** (hit/miss, eviction under budget, invalidation, oversized reject, shard geometry/spread, CLOCK second chance, ring consistency, global budget, shared allocation on hit). `invalidate_file` becomes infallible (RAM only).

`Storage::open`: `create_dir_all(dir)`, `BlockStore::open(dir, cfg.store)`, `Catalog::open(dir/"catalog.redb")`, `RamTier::new(cfg.ram_tier_bytes)`. (Reconciliation is Task 5.)

- [ ] **Step 1: Failing tests** in `catalog.rs` and `storage.rs` `#[cfg(test)]` modules:

```rust
#[test]
fn layers_are_named_unique_and_persistent() {
    let dir = tempfile::tempdir().unwrap();
    let id = { let c = Catalog::open(&dir.path().join("c.redb")).unwrap();
               let id = c.create_layer("prof").unwrap();
               assert!(matches!(c.create_layer("prof"), Err(StorageError::LayerExists(_))));
               c.commit_durable().unwrap(); id };
    let c = Catalog::open(&dir.path().join("c.redb")).unwrap();
    assert_eq!(c.layer_id("prof").unwrap(), Some(id));
    assert!(c.get(id, "").unwrap().is_some_and(|r| r.kind == vfs_provider::KIND_DIR));
}

#[test]
fn children_are_direct_only_and_case_preserving() {
    let dir = tempfile::tempdir().unwrap();
    let c = Catalog::open(&dir.path().join("c.redb")).unwrap();
    let l = c.create_layer("p").unwrap();
    let file = |name: &str| EntryRec { name: name.into(), kind: vfs_provider::KIND_FILE, guid: [1; 16], len: 1, mtime: 0 };
    let dirr = |name: &str| EntryRec { name: name.into(), kind: vfs_provider::KIND_DIR, guid: [0; 16], len: 0, mtime: 0 };
    c.put(l, "saves", &dirr("Saves"), false).unwrap();
    c.put(l, "saves/one.ess", &file("One.ess"), false).unwrap();
    c.put(l, "saves/sub", &dirr("Sub"), false).unwrap();
    c.put(l, "saves/sub/deep.ess", &file("deep.ess"), false).unwrap();
    let mut names: Vec<_> = c.children(l, "saves").unwrap().into_iter().map(|r| r.name).collect();
    names.sort();
    assert_eq!(names, vec!["One.ess", "Sub"]);
    assert_eq!(c.children(l, "").unwrap().len(), 1);
}

#[test]
fn rename_moves_a_subtree() {
    let dir = tempfile::tempdir().unwrap();
    let c = Catalog::open(&dir.path().join("c.redb")).unwrap();
    let l = c.create_layer("p").unwrap();
    let dirr = |name: &str| EntryRec { name: name.into(), kind: vfs_provider::KIND_DIR, guid: [0; 16], len: 0, mtime: 0 };
    let file = |name: &str| EntryRec { name: name.into(), kind: vfs_provider::KIND_FILE, guid: [7; 16], len: 3, mtime: 0 };
    c.put(l, "a", &dirr("A"), false).unwrap();
    c.put(l, "a/x", &file("x"), false).unwrap();
    c.rename(l, "a", "b", "B").unwrap();
    assert!(c.get(l, "a").unwrap().is_none() && c.get(l, "a/x").unwrap().is_none());
    assert_eq!(c.get(l, "b").unwrap().unwrap().name, "B");
    assert_eq!(c.get(l, "b/x").unwrap().unwrap().guid, [7; 16]);
}

#[test]
fn ids_are_prefixed_and_classified() {
    let g = new_guid();
    assert!(matches!(classify_store_id(&layer_file_id(&g)), StoreIdKind::Layer(x) if x == g));
    assert!(matches!(classify_store_id(&cache_file_id(&[9; 16])), StoreIdKind::Cache(x) if x == [9; 16]));
    assert!(matches!(classify_store_id(b"other"), StoreIdKind::Foreign));
    assert_ne!(new_guid(), new_guid());
}

#[test]
fn storage_opens_twice_in_sequence_but_not_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    let s = Storage::open(dir.path(), StorageConfig::default()).unwrap();
    assert!(Storage::open(dir.path(), StorageConfig::default()).is_err(), "the block store lock must hold");
    s.close().unwrap();
    Storage::open(dir.path(), StorageConfig::default()).unwrap();
}
```

- [ ] **Step 2:** run → compile errors.
- [ ] **Step 3: Implement** per the interfaces above. `Cargo.toml`: `license = "GPL-3.0-only"`, deps `vfs-block-store`, `vfs-provider`, `vfs-core`, `redb = "4"` (match the block store's redb major), `uuid = { version = "1", features = ["v4"] }`, `blake3 = "1"`, `tracing = "0.1"`; dev-dep `tempfile = "3"`.
- [ ] **Step 4:** `cargo test -p vfs-storage` → pass; clippy clean; Windows-target lint clean for `-p vfs-storage`.
- [ ] **Step 5: Commit** `feat(storage): the vfs-storage crate — catalog, RAM tier and Storage::open`

---

### Task 3: `CachedSource` and eviction

**Files:** Create `rust/crates/vfs-storage/src/cached.rs`, `src/evict.rs`; modify `src/storage.rs`, `src/lib.rs`.

**Interfaces:**
- Consumes Task 2 (`Storage`, `Catalog::cache_*`, `RamTier`, `cache_file_id`).
- Produces:
  ```rust
  pub struct SourceKey(pub String); // stable across runs: remote endpoint, or a config cache_key
  impl Storage {
      pub fn cached(self: &std::sync::Arc<Self>, source: std::sync::Arc<dyn vfs_provider::Provider>, key: SourceKey) -> std::sync::Arc<dyn vfs_provider::Provider>;
      pub fn cache_stats(&self) -> CacheStats; // hits (ram+store), misses, ram_evicts, store_hits, bytes_from_cache, bytes_from_source, ram_bytes, cached_logical_bytes
      pub fn enforce_cache_budget(&self) -> Result<u64, StorageError>; // returns files evicted
  }
  ```
- `cached` returns `source` unchanged unless `caps.immutable && caps.slow && caps.access >= Access::Read`.

**Behaviour:**
- **File id** at `open`: `blake3` over length-prefixed `[key.0, normalized rel path, size.to_le_bytes(), version]`, truncated to 16 bytes → `cache_file_id`. `version` = `mtime.to_le_bytes()` from the inner `getattr` (the remote `file_id` is not plumbed through the `Provider` trait; Task 8's remote test proves version change via size/mtime). Root id is part of the hash (`p.root.0`).
- **`open`**: delegate `open` to the inner provider (keep its handle for fetches); if the store lacks the id, `store.set_len(id, size)`. Returns a handle mapping to `(inner handle, id, size)`. Directories and `getattr`/`readdir` pass through.
- **`read_at`**: for each block in range: RAM tier → else `store.read` of that block → if missing, fetch the whole block from the inner handle (loop short reads; the last block is exactly its remaining length), `write_blocks`, put in RAM, count bytes_from_source; copy out the requested bytes. Coalesce concurrent misses: a `Mutex<HashMap<([u8;17], u64), Arc<Condvar-or-OnceLock>>>` so a second reader of the same block waits for the first fetch.
- **Access tracking:** record `(id → now_minute, logical_bytes = size)` in an in-memory map; commit it with `cache_put_many` when a minute has passed since the last commit and in `Storage::close`.
- **Eviction:** after a fetch that adds bytes, if `cached_logical_bytes > cache_max_bytes`, run `enforce_cache_budget` on a background thread (at most one at a time): sort `cache_all()` by `last_access_min` ascending, `store.delete` + `cache_remove` + `ram.invalidate_file` until `≤ 90%` of budget, then `store.compact(CompactOptions::default())`. A file with a live open handle is skipped.
- Errors map to `vfs_provider` statuses (`StorageError::to_status`).

- [ ] **Step 1: Failing tests** (`cached.rs` tests; a counting test source wrapping `vfs_provider::conformance::MemFixture` with caps `{access: Read, immutable: true, slow: true, preferred_block: None, case: <MemFixture's>}` and a `reads: AtomicU64`):

```rust
#[test] fn conformance_through_the_cache() {
    let (s, _d) = temp_storage();
    vfs_provider::assert_conformance(s.cached(slow_fixture(), SourceKey("k".into())));
}
#[test] fn second_read_is_served_from_the_store() {
    let (s, _d) = temp_storage(); let src = slow_fixture();
    let p = s.cached(src.clone(), SourceKey("k".into()));
    read_all(&p, "a.txt"); let before = src.reads();
    read_all(&p, "a.txt"); assert_eq!(src.reads(), before, "no source read on the second pass");
}
#[test] fn survives_reopen() { /* read via one Storage, close, reopen, read: zero source reads */ }
#[test] fn fast_or_mutable_sources_are_not_wrapped() {
    let (s, _d) = temp_storage();
    let fast: Arc<dyn Provider> = Arc::new(vfs_provider::conformance::MemFixture::new());
    assert!(Arc::ptr_eq(&s.cached(fast.clone(), SourceKey("k".into())), &fast));
}
#[test] fn version_change_is_a_new_cache_file() {
    // Serve "a.txt" = "hello", read it; change the fixture to "hello!!" (new size); read → "hello!!".
}
#[test] fn concurrent_misses_fetch_once() { /* 8 threads read the same uncached block; source reads == 1 */ }
#[test] fn eviction_keeps_the_budget_and_evicts_least_recent_first() {
    // cache_max_bytes = 3 files' worth; read f1, f2, f3, then f1 again (touch), then f4 → f2 evicted, f1 kept.
}
```

(Write `temp_storage`, `slow_fixture`, `read_all` as small helpers in the test module; `MemFixture` lets tests add files — if it cannot be mutated, use a local `HashMap`-backed test provider with the same caps.)
- [ ] **Step 2:** run → fail. **Step 3:** implement. **Step 4:** `cargo test -p vfs-storage`, clippy, Windows lint → clean. **Step 5: Commit** `feat(storage): CachedSource — pull-through caching of immutable, slow sources`

---

### Task 4: `LayerProvider`

**Files:** Create `rust/crates/vfs-storage/src/layer.rs` (namespace ops) and `src/layer_io.rs` (per-file state, handles, block read-modify-write); modify `src/storage.rs`, `src/lib.rs`.

**Interfaces:**
- Consumes Task 2 (`Catalog`, `EntryRec`, `new_guid`, `layer_file_id`, `RamTier`, `Storage`).
- Produces: `impl Storage { pub fn layer(self: &Arc<Self>, name: &str) -> Result<Arc<dyn Provider>, StorageError> }` — opens the named layer, **creating it if absent**; the returned provider's capabilities are `{access: ReadWrite, immutable: false, slow: false, preferred_block: Some(block_size), case: CaseMatch::Insensitive}`. Also `pub fn layers_in_use(&self) -> Vec<String>` (names with a live `LayerProvider`), used by Task 5.

**Behaviour:**
- Paths: `rel` folded with `vfs_core::fold` for keys; original spelling stored in `EntryRec.name`. Parents must exist as directories (else `ST_NOT_FOUND`); `mkdir` creates one level; `remove` of a non-empty directory → `ST_EXISTS` is wrong — match `DiskProvider`/`RwMemFixture`'s behaviour for the conformance suite (read `vfs-provider/src/conformance.rs` `assert_writable` and follow what it asserts).
- **Per-file state** shared by all handles of a GUID: `Arc<Mutex<FileState{ len: u64, dirty: BTreeMap<u64 /*block*/, Vec<u8>>, committed_len: u64 }>>` in a `HashMap<Guid, Weak<..>>`.
- **Read:** for each block: dirty buffer → RAM tier → `store.read`; blocks at or beyond `committed_len` but below `len` read as zeros; a `missing` range below `committed_len` → `ST_IO_ERROR` with `tracing::error!(layer, path, block, "layer block missing: corruption")`.
- **Write (`write_at`, and `OPEN_APPEND` is handled by the Director):** for each touched block: start from the dirty copy, else the committed block (read as above, zeros past `committed_len`), patch, store in `dirty`; `len = max(len, end)`. If `dirty.len() > 4`, commit.
- **`set_len`:** drop dirty blocks past the new end; truncate the dirty copy of the new tail block if present, else load the committed tail, truncate it and mark it dirty; set `len`; commit.
- **Commit** (`commit(&mut FileState)`): if `len != committed_len`, capture every committed block the store will drop — on grow, the old tail block if it was short; on shrink, the new tail block — into `dirty` first (from the store), then `store.set_len(id, len)`; then for every block index from `min(old_tail, first_dirty)` needed so that **every block below `len` is present** (dirty blocks, recaptured tails, and explicit zero blocks for gaps between `committed_len` and `len`), `write_blocks` in runs of consecutive blocks; `ram.invalidate_file(id)`; clear `dirty`; `committed_len = len`; update the catalog row's `len` (non-durable).
- **`flush(h)` and `close(h)` of a handle that wrote:** commit, `store.flush()`, `catalog.commit_durable()`.
- **Create** (`open` with `OPEN_CREATE` on an absent path): new GUID, catalog row `{kind FILE, len 0}` (non-durable), then `store.set_len(id, 0)`. `OPEN_TRUNC`: `set_len(0)`. `OPEN_EXCL` on an existing path → `ST_EXISTS`.
- **`rename`:** `catalog.rename` (subtree); open handles keep working (they hold the GUID). Refuse rename onto an existing directory per `assert_writable`.
- **`remove`:** file → `catalog.remove` then `store.delete(id)` (if no open handle; else defer the store delete to the last close) and `ram.invalidate_file`; empty directory → `catalog.remove`.
- **`set_attr`:** `size` → as `set_len` via a temporary state; `mtime` → catalog row.
- `readdir` → `catalog.children` mapped to `DirEntry{name, stat{kind, size: len, mtime}}`; `getattr` → the row (a file's `size` is its live `len` if it has state).

- [ ] **Step 1: Failing tests** (`layer.rs` tests):

```rust
#[test] fn conformance() {
    let (s, _d) = temp_storage();
    let p = s.layer("conf").unwrap();
    seed_fixture(&p); // create sub/, write FIXTURE_FILES through open(CREATE|WRITE)/write_at/close
    vfs_provider::assert_conformance(p);
}
#[test] fn partial_block_write_preserves_neighbours() {
    // 3 blocks of 0xAA; write b"XYZ" at block_size + 10; read all: only those 3 bytes changed.
}
#[test] fn gap_reads_as_zeros() {
    // create; write b"end" at 5 * block_size + 7; read [0, 5*bs+10): zeros then "end"; also after close+reopen.
}
#[test] fn truncate_then_grow_zero_fills() { /* write 3 blocks of 0xAA, set_len(bs+5), set_len(3*bs); tail past bs+5 is zeros */ }
#[test] fn second_handle_sees_uncommitted_writes() { /* h1 writes 10 bytes (no close); h2 opened on same path reads them */ }
#[test] fn rename_moves_without_copying() { /* write 2 blocks, rename dir containing it; store.stats unflushed/appended bytes unchanged by the rename; content intact */ }
#[test] fn closed_file_is_durable_across_storage_reopen() { /* write+close, drop Storage without close(), reopen: bytes present */ }
#[test] fn missing_committed_block_is_an_io_error() { /* write+close; delete the store file id directly via storage.store; read → ST_IO_ERROR */ }
#[test] fn case_insensitive_lookup_preserves_case() { /* create "Saves/One.ESS"; getattr("saves/one.ess") finds it; readdir shows "One.ESS" */ }
#[test] fn overlay_copy_up_into_a_layer() {
    // vfs_compose::OverlayProvider::from_arcs(base = RwMemFixture-with-fixture-tree read-only, upper = layer);
    // open a.txt for write, write at 0, close; read back through the overlay; base untouched.
    // (dev-dependency on vfs-compose)
}
```
- [ ] **Step 2:** fail. **Step 3:** implement. **Step 4:** `cargo test -p vfs-storage`, clippy, Windows lint. **Step 5: Commit** `feat(storage): LayerProvider — named, persistent read-write layers in the block store`

---

### Task 5: Reconciliation and layer management

**Files:** Create `rust/crates/vfs-storage/src/reconcile.rs`, `src/manage.rs`; modify `src/storage.rs`.

**Interfaces (Produces):**
```rust
pub struct LayerInfo { pub name: String, pub files: u64, pub logical_bytes: u64 }
pub struct ReconcileReport { pub emptied_files: Vec<(String, String)> /*layer, path*/, pub orphans_deleted: u64, pub cache_rows_dropped: u64 }
impl Storage {
    pub fn last_reconcile(&self) -> &ReconcileReport;       // what Storage::open's reconciliation did
    pub fn layers(&self) -> Result<Vec<LayerInfo>, StorageError>;
    pub fn export_layer(&self, name: &str, dir: &std::path::Path) -> Result<u64 /*files*/, StorageError>;
    pub fn import_layer(&self, dir: &std::path::Path, name: &str) -> Result<u64, StorageError>; // LayerExists if present
    pub fn delete_layer(&self, name: &str) -> Result<(), StorageError>;                        // LayerInUse if a live LayerProvider
    pub fn stats(&self) -> StorageStats; // cache_stats() + pack_bytes, live_bytes, layer_count
}
```

**Behaviour:**
- `Storage::open` runs reconciliation after opening both halves, per spec §6: for each `(layer, path, guid)` in `catalog.all_layer_guids()` whose `layer_file_id` the store lacks (`store.stat` → `None`): `store.set_len(id, 0)`, set the row's `len = 0`, record it, `tracing::warn!`. Build a set of known ids (layer GUIDs + `cache_all` ids); for each `store.file_ids()` entry not in it → `store.delete`. For each cache row whose id the store lacks → `cache_remove`. If anything was deleted → `store.compact(default)`. `store.flush()` + `catalog.commit_durable()`.
- `export_layer`: walk the layer; create directories; write each file's bytes (read through a `LayerProvider`); skip names ending in the overlay's whiteout marker (`.wh.` prefix — read `vfs-compose/src/overlay.rs` for the exact marker constant and reuse it; add `vfs-compose` as a dependency only if it exports the constant, else duplicate it with a comment naming the source). Refuse a non-empty target dir.
- `import_layer`: create the layer, copy files/dirs in through a `LayerProvider`, flush.
- `delete_layer`: refuse if in `layers_in_use()`; `catalog.drop_layer` → `store.delete` each GUID → `commit_durable` → compact.

- [ ] **Step 1: Failing tests:**

```rust
#[test] fn a_catalog_row_without_store_data_becomes_empty() { /* catalog.put a file row with a fresh guid, never set_len in store; reopen Storage; getattr size 0; report lists it */ }
#[test] fn store_orphans_are_deleted() { /* store.set_len(layer_file_id(&new_guid()), 10) directly with no catalog row; reopen; file_ids no longer has it */ }
#[test] fn closed_file_survives_reopen_without_close() { /* write+close via layer; std::mem::forget the Arc<Storage> is not possible for the lock — instead drop without Storage::close and reopen; bytes present */ }
#[test] fn export_import_round_trip() { /* layer with nested dirs + a whiteout marker file; export → dir has files, no marker; import into new name → identical tree */ }
#[test] fn delete_refuses_a_live_layer_and_frees_it_otherwise() { /* hold layer("x") → LayerInUse; drop provider → delete ok; layers() lacks it */ }
```
- [ ] Steps 2–5 as usual. **Commit** `feat(storage): reconcile catalog and store at open; export, import and delete layers`

---

### Task 6: Control plane — config, proto, env

**Files:** `rust/crates/vfs-control/src/config.rs`, `rust/crates/vfs-control/proto/director.proto`, `rust/crates/vfs-env/src/lib.rs`, `rust/crates/vfs-source/src/lib.rs`, every `match` over `SourceSpec`/`source_spec::Kind` (at least `vfs-directord/src/service.rs:276` `pb_to_source_spec`, `vfs-directord/src/lib.rs:~281` `parse_source_flag`, `:~445` in `apply_session_config`).

**Interfaces (Produces):**
- `vfs_control::SourceSpec::Layer { name: String }` (serde tag `type = "layer"`); `SourceEntry.cache_key: Option<String>` (`#[serde(default)]`).
- `validate_roots` refuses a `layer` source without `write_layer = true` ("a layer source is a write layer; set write_layer = true") and a `write_layer` source that is neither `disk` nor `layer`.
- `director.proto`: `SourceSpec.oneof kind` gains `LayerSource layer = 5;` with `message LayerSource { string name = 1; }`; `AddSourceReq` gains `string cache_key = 7;`; `StatsResp` gains `uint64 store_pack_bytes = 12; uint64 store_live_bytes = 13; uint64 cache_logical_bytes = 14; uint32 layers = 15;`; new RPCs `rpc ListLayers(Empty) returns (LayerList); rpc ExportLayer(LayerPathReq) returns (LayerCount); rpc ImportLayer(LayerPathReq) returns (LayerCount); rpc DeleteLayer(LayerNameReq) returns (Empty);` with `message LayerInfo { string name = 1; uint64 files = 2; uint64 logical_bytes = 3; } message LayerList { repeated LayerInfo layers = 1; } message LayerPathReq { string name = 1; string dir = 2; } message LayerNameReq { string name = 1; } message LayerCount { uint64 files = 1; }`.
- `vfs_env::STORAGE_DIR = "VFS_STORAGE_DIR"` (`Kind::Behaviour`, default `"$VFS_HOME/storage"`).
- `SessionConfig`'s `[cache]` block: keep the field so old configs parse; `vfs_control::load` emits `tracing::warn!` (or `eprintln!` if `vfs-control` has no tracing) "the [cache] block is ignored; the cache is daemon-wide: vfs daemon --storage-dir/--cache-max-gib".
- `vfs_source::build_provider(&SourceSpec::Layer{..})` → `BuildError` "a layer source needs the daemon's storage".
- `parse_source_flag("layer:NAME")` → `SourceSpec::Layer`; `--write-layer layer:NAME` → a `Layer` write-layer entry (`write_layer_flag_entry` branches on the `layer:` prefix).
- Daemon side of the new RPCs: add **stubs returning `Status::unimplemented("storage lands in Task 7")`** in `service.rs` so the crate compiles; Task 7 fills them in.

- [ ] **Step 1: Failing tests** in `config.rs` tests: parse `type = "layer"` + `name` + `write_layer = true`; `cache_key` parses; `validate_roots` rejects a layer without `write_layer` and a zip write layer; old config with `[cache]` still loads. In `vfs-directord/src/lib.rs` tests: `parse_source_flag("layer:prof")` and `write_layer_flag_entry("layer:prof")`.
- [ ] Steps 2–4: `cargo test -p vfs-control -p vfs-env -p vfs-source -p vfs-directord`; clippy; Windows lint for `vfs-directord`; `bin/regen-protocol && git diff --exit-code resources/`.
- [ ] **Step 5: Commit** `feat(control): layer sources, cache_key, storage stats and layer RPCs on the wire`

---

### Task 7: Daemon and CLI on `vfs-storage`; delete `vfs-cache`

**Files:** `rust/crates/vfs-directord/{Cargo.toml,src/registry.rs,src/service.rs,src/lib.rs,src/main.rs,tests/composition.rs}`, `rust/crates/vfs-embed/{Cargo.toml,src/lib.rs}`, delete `rust/crates/vfs-cache/`, `rust/Cargo.toml`, `.github/workflows/ci.yml` (drop `-p vfs-cache`, add `-p vfs-storage`), README rows.

**Interfaces:**
- Consumes: Tasks 2–6.
- `SessionRegistry::new()` → no storage (sources uncached; `layer` refused with "this daemon has no storage"); `SessionRegistry::with_storage(Arc<vfs_embed::Storage>)`; `SessionRegistry::storage() -> Option<&Arc<Storage>>`. `with_cache`/`cache()` are removed.
- `vfs-embed` re-exports `vfs_storage::{Storage, StorageConfig, SourceKey, StorageError, LayerInfo}`; removes the `vfs_cache` re-exports and dependency.

**Behaviour:**
- `add_source`: build the provider as today; if storage is present → `storage.cached(backend, SourceKey(cache_key.unwrap_or(<spec identity>)))` where spec identity is the remote endpoint (for other kinds `cached` returns the source unchanged anyway, so pass the path).
- `set_write_layer` with `SourceSpec::Layer{name}` → `storage.layer(&name)?` as the upper; the registry records the layer name against the session (for `delete` refusal — `Storage::layers_in_use` already covers live providers).
- Daemon startup (`serve_daemon_until`): storage dir = `--storage-dir` flag, else `vfs_env::STORAGE_DIR`, else `<VFS_HOME>/storage` where VFS_HOME resolves as `vfs_env::HOME`, else `$XDG_DATA_HOME/aether-vfs`, else `$HOME/.local/share/aether-vfs` (unix) / `%LOCALAPPDATA%\aether-vfs` (Windows). `--cache-max-gib N` sets `cache_max_bytes`. `Storage::open` failure (including `Locked`) → the daemon exits with an error naming the directory and the flag. Print the reconcile report if non-empty. On shutdown drain: tear down sessions, then `Storage::close`.
- `Stats` RPC from `storage.stats()` per spec §3's mapping (zeros without storage); new RPCs implemented (`ListLayers`, `ExportLayer`, `ImportLayer`, `DeleteLayer`) via `spawn_blocking`.
- CLI: `vfs layer list | export NAME DIR | import DIR NAME | delete NAME`; `vfs daemon --storage-dir DIR --cache-max-gib N`; `vfs stats` prints the new fields.
- `tests/composition.rs` `registry_cache_hits_on_second_read`: rewrite to use `SessionRegistry::with_storage(temp)` and a slow+immutable test provider; assert the second read is a hit in `storage.stats()`.
- Delete `vfs-cache` and every reference (grep `vfs_cache`, `vfs-cache`, `CachingProvider`, `BlockCache`), including docs rows (`README.md`, `rust/README.md`, `rust/docs/architecture.md` — replace the vfs-cache section with a short `vfs-storage` paragraph pointing at the spec).

- [ ] **Step 1: Failing tests** (registry tests): `layer_write_layer_persists_across_registries` (registry A with storage S: create session, `set_write_layer` layer "p", write a file through the session kernel, teardown; registry B with the same S (after close/reopen): new session with layer "p" reads the file); `layer_source_without_storage_is_refused`; `stats_report_storage`.
- [ ] Steps 2–4: `cargo test -p vfs-directord -p vfs-embed -p vfs-storage --no-fail-fast`; the full Linux CI crate list; clippy Linux; Windows lint `--workspace`; `bin/regen-protocol` clean. The Proton e2e tests still pass (run both, `VFS_HOME=$HOME/.local/share/aether-vfs`, with `VFS_STORAGE_DIR` pointed at a scratch dir).
- [ ] **Step 5: Commit(s)** `feat(directord): the daemon runs on vfs-storage` and `chore: delete vfs-cache`

---

### Task 8: End-to-end, CI and docs

**Files:** Create `rust/crates/vfs-directord/tests/storage_pull_through.rs`; modify `rust/crates/vfs-directord/tests/proton_cli.rs`, `.github/workflows/ci.yml`, `README.md`.

- [ ] **Step 1: `storage_pull_through.rs`** (portable; runs on both OSes in CI): start an in-process `vfs_source` gRPC server (`ProviderSourceService` over a test provider with caps `{Read, immutable: true, slow: true}` serving `data.bin` = 1 MiB of a pattern; see `vfs-source/src/bin/vfs-source-plugin.rs` for server setup) and an in-process daemon (`SessionRegistry::with_storage(temp)` + `DirectorService` as in `e2e.rs`'s registry tests); `apply_session_config` with one `remote` source; read `data.bin` twice through the session kernel; assert byte equality, `Stats.cache_misses > 0` after pass 1 and `cache_hits > 0` with no new misses after pass 2; then change the served file (new size) and assert the new bytes are served.
- [ ] **Step 2: `proton_cli.rs` layer variant** — a second `#[test] #[ignore]` `vfs_up_then_exec_with_a_layer_write_layer`: same as the existing test but root 1's write layer is `type = "layer"`, `name = "e2e-<pid>"`, `VFS_STORAGE_DIR` = a temp dir for the daemon. After the exec: `vfs down`; stop the daemon; start a new one (next `vfs` command auto-spawns) and `vfs layer export e2e-<pid> <dir>`; assert `<dir>/save.txt == "saved"`; `vfs layer delete e2e-<pid>`. Cleanup as the existing test (guard; kill daemons; remove prefix and temp dirs).
- [ ] **Step 3: CI** — `proton-linux` runs the new ignored test; the portable job's `-p` list has `vfs-storage` (Task 7 added it) — verify.
- [ ] **Step 4: README** — the daemon section documents storage (`$VFS_HOME/storage`, `--storage-dir`, `VFS_STORAGE_DIR`, `--cache-max-gib`), which sources are cached, `type = "layer"` write layers, and `vfs layer …`.
- [ ] **Step 5:** run the new tests (Proton one with `bin/build-windows` first); commit `test(directord): storage pull-through and layer write layers end to end; docs`

---

## Finish

- [ ] Whole-branch review; one fix wave; re-review.
- [ ] Linux full suite + clippy; Windows lint `--workspace`; `bin/regen-protocol` clean; both Proton e2e tests.
- [ ] Push `storage`, CI green, merge `--no-ff` into `master`, push, confirm `master` CI.
