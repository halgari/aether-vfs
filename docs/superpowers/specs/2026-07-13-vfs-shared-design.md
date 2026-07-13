# vfs-shared Snapshot Layout — Design Spec

**Status:** Approved (design), ready for implementation planning
**Date:** 2026-07-13
**Slice:** Second implementable slice of the Usermode VFS project — the
bitness-neutral shared-memory **snapshot layout** (tree only), plus the
builder, the lock-free reader, and the seqlock publish/read protocol.
**Parent docs:** *Out-of-Process (IPC) Architecture* (§2 region A, §9, §13),
*Rust Implementation Guide* (§3, H6), *Design Document* (G9/G12).
**Depends on:** `vfs-core` (slice 1, done) — only behind the optional `bridge`
feature.

---

## 1. Context & positioning

In the out-of-process architecture, the authoritative virtual tree lives in a
shared-memory **snapshot** region: the server is the sole writer; injected shims
are lock-free readers that resolve `getattr`/`readdir`/re-open **with zero IPC
round-trips** (IPC doc §2 region A, §4). `vfs-shared` defines that snapshot's
`#[repr(C)]`, bitness-neutral layout and the code to build and read it.

This slice covers the **tree snapshot only**. The cache index
(content-hash → backing-id + refcount) is deferred to the materialization/server
slice that exercises it. The control ring / data arena / sync block are
`vfs-ipc`'s responsibility (M0), not here.

### Decisions locked during brainstorming

1. **Byte-buffer, OS-independent.** `vfs-shared` operates on caller-provided
   `&[u8]`/`&mut [u8]`. It knows nothing of `CreateFileMapping`/`MapViewOfFile`;
   the server/shim/`vfs-ipc` own the actual segment. Stable Rust, fully
   unit-testable with a `Vec<u8>` fixture — mirrors `vfs-core`'s purity.
2. **Tree snapshot only.** No cache index, no ring/arena/sync regions this slice.
3. **Generation/seqlock read protocol.** A header generation counter + immutable
   wholesale republish; readers acquire-read-revalidate and retry on change.
   Cross-process memory **reclamation** is out of scope (single reused buffer +
   seqlock needs none; RCU/double-buffering for large mutation is a future
   concern).
4. **Approach A — dependency-free reader + feature-gated bridge.** The core
   (layout + low-level builder + reader) depends on nothing. The reader is
   **fold-free** (raw byte compares; caller passes already-folded keys). The
   `VfsTree → snapshot` bridge lives behind an optional `bridge` feature that
   pulls `vfs-core`; the server and the round-trip test enable it, the **shim
   never does** (keeps provider logic out of the shim, G12 spirit).
5. **No `bytemuck`, no `unsafe`.** (De)serialization is manual little-endian via
   `offset_of!`-derived field offsets and `from_le_bytes`/`to_le_bytes`. Works on
   any `&[u8]` regardless of alignment; zero dependencies in the default build.

---

## 2. Scope & crate boundary

`crates/vfs-shared`, stable Rust, `#![forbid(unsafe_code)]`.

### In scope (this slice)

- The `#[repr(C)]` snapshot layout: `Header`, `SnapNode`, `SnapChild`, string
  blob; compile-time size/align assertions (D0/G9).
- `SnapshotBuilder` — post-order low-level builder producing a complete image.
- `SnapshotReader` — validate + `getattr`/`resolve`/`readdir` over `&[u8]`, every
  access bounds-checked (torn-read safe).
- `publish` / `read_stable` — the seqlock writer/reader free functions.
- Feature `bridge`: `bridge::flatten(&vfs_core::VfsTree) -> Vec<u8>` + a small
  additive read-only walk API on `vfs-core`.

### Explicitly out of scope (later slices)

- Cache index (content-hash → backing-id + refcount) and its concurrency.
- Control ring / data arena / sync block (`vfs-ipc`, M0).
- OS shared-memory mapping (`CreateFileMapping`/`MapViewOfFile`) — server/shim.
- Cross-process memory reclamation / double-buffering / RCU.
- Runtime tree mutation (already deferred in `vfs-core`); the snapshot stores an
  **already-merged** tree — tombstones are resolved by `vfs-core` at build time
  and do not appear in the snapshot.

---

## 3. Segment layout (`#[repr(C)]`, little-endian, bitness-neutral)

Buffer regions, in order: **`[Header][Node array][Child array][String blob]`**.
All `*_off` are absolute byte offsets from buffer start; `child_first` is an
element index into the child array. Every field is fixed-width
(`u32`/`u64`/`i64`/`[u8;N]`) — no pointers, no `usize` (H6/G9). Little-endian,
same-machine only (Windows x86/x64 are both LE; shared memory is same host).

```
Header (48 bytes, 8-aligned)
  magic:u32@0        // 0x56465353  b"VFSS"
  version:u32@4      // layout version, starts at 1
  generation:u64@8   // seqlock counter (even = stable, odd = write in progress)
  total_len:u32@16   // total image length; must be <= buffer len
  root_node:u32@20   // index of the root SnapNode
  node_count:u32@24
  nodes_off:u32@28   // = 48
  child_count:u32@32
  children_off:u32@36
  strings_len:u32@40
  strings_off:u32@44

SnapNode (80 bytes, 8-aligned)     // one fixed struct; kind distinguishes dir/file
  kind:u8@0 (0=dir, 1=file)  _pad0:[u8;3]@1  layer:u32@4
  name_off:u32@8   name_len:u32@12            // display name (case preserved)
  child_first:u32@16  child_count:u32@20      // dir: run into child array
  source_off:u32@24   source_len:u32@28       // file: opaque SourceId bytes
  size:u64@32  mtime:i64@40  cache_key:[u8;32]@48

SnapChild (16 bytes)               // an entry in a parent's sorted child run
  folded_off:u32@0  folded_len:u32@4          // folded name (binary-search key)
  node:u32@8  _pad:u32@12
```

- **Children** of a dir occupy `[child_first .. child_first+child_count)` in the
  child array, **sorted by folded name** → binary search on lookup, and already
  case-insensitively ordered for `readdir` (no read-time sort).
- **Names stored twice**: display on `SnapNode`, folded on `SnapChild`. This
  keeps the reader fold-free (raw byte comparison of folded keys).
- **Files** carry the opaque `SourceId` bytes (`source_off/len` into the blob),
  `size`, `mtime`, winning `layer`, and the 32-byte `cache_key` — the full
  `vfs-core` file resolution (the future cache index will key off `cache_key`).
- **Offsets are single-sourced** via `core::mem::offset_of!` on the `#[repr(C)]`
  structs; builder and reader both derive field positions from it so they cannot
  drift. Compile-time asserts guard sizes/alignment:
  ```rust
  const _: () = assert!(size_of::<Header>() == 48 && align_of::<Header>() == 8);
  const _: () = assert!(size_of::<SnapNode>() == 80 && align_of::<SnapNode>() == 8);
  const _: () = assert!(size_of::<SnapChild>() == 16);
  ```
  These compile under any target triple; CI later also compiles them under
  `i686-pc-windows-msvc` to prove x86-readiness (D0, G9).
- **Region offsets**: `nodes_off = 48`; `children_off = nodes_off +
  node_count*80`; `strings_off = children_off + child_count*16`; `total_len =
  strings_off + strings_len`. Each region start is 8-aligned by construction.

---

## 4. Builder API (low-level, post-order)

The builder accumulates a string blob, node array, and child array, then
serializes. Dirs reference already-built children → **bottom-up (post-order)**
construction.

```rust
pub struct SnapshotBuilder { /* strings: Vec<u8>, nodes: Vec<..>, children: Vec<..>, root: u32 */ }

impl SnapshotBuilder {
    pub fn new() -> Self;

    /// Add a file leaf; returns its node index.
    pub fn add_file(&mut self, display: &str, source: &[u8],
                    size: u64, mtime: i64, layer: u32, cache_key: [u8; 32]) -> u32;

    /// Add a directory whose children are already built. `children` =
    /// (folded_name, node_index) pairs; the builder sorts them by folded name
    /// and stores the run. Returns the dir's node index.
    pub fn add_dir(&mut self, display: &str, children: &[(String, u32)]) -> u32;

    pub fn set_root(&mut self, node: u32);

    /// Serialize to a complete snapshot image. `generation` is written 0 (even,
    /// stable); root/offsets/counts are finalized here.
    pub fn finish(self) -> Vec<u8>;
}
```

- The caller passes **folded** child names (the fold contract, §6); `display`
  names are stored verbatim. Interned strings may be deduplicated by the builder
  (optional optimization; not required for correctness).
- Pure, `Vec`-allocating, no concurrency. Serialization writes each field at its
  `offset_of!` position into a zeroed `[u8; size_of::<T>()]` via `to_le_bytes`,
  so the bytes match the `#[repr(C)]` layout exactly (padding bytes stay zero).

---

## 5. Reader API, seqlock protocol & torn-read safety

```rust
pub struct SnapshotReader<'a> { bytes: &'a [u8] }

impl<'a> SnapshotReader<'a> {
    /// Validate magic/version/region bounds (cheap, no scan). Alignment-free.
    pub fn open(bytes: &'a [u8]) -> Result<Self, LayoutError>;

    pub fn generation(&self) -> u64;
    pub fn root(&self) -> u32;

    /// All take ALREADY-FOLDED path components (fold contract §6). Root = &[].
    pub fn getattr(&self, folded: &[&str]) -> Option<SnapStat>;
    pub fn resolve(&self, folded: &[&str]) -> SnapResolution;
    pub fn readdir(&self, folded: &[&str]) -> Result<Vec<SnapDirEntry>, ReadError>;
}

pub enum NodeKind { Dir, File }
pub struct SnapStat { pub kind: NodeKind, pub size: u64, pub mtime: i64 }
pub struct SnapDirEntry { pub name: String, pub kind: NodeKind, pub size: u64, pub mtime: i64 }
pub enum SnapResolution {
    File { source: Vec<u8>, size: u64, mtime: i64, layer: u32, cache_key: [u8; 32] },
    Dir,
    NotFound,
}
```

**Torn-read safety (load-bearing).** Every access — node index, child index,
string `(off,len)` — is **bounds-checked against `bytes.len()`**. Any
out-of-range value yields `None`/`NotFound`/`ReadError`, never a panic or OOB
read. This is what makes it safe to read *possibly-torn* shared bytes during a
concurrent republish and discard the result on revalidation.

**Lookup:** navigate from `root_node`, at each component binary-search the
current dir's child run by folded name (byte compare), following `node`. A
component under a file node ⇒ `NotFound`/`NotADirectory` as appropriate.

**Seqlock free functions:**

```rust
/// Publish an image into a shared buffer under the seqlock:
///   1. store generation |= 1 (odd, Release)          // readers now retry
///   2. copy every byte of `image` EXCEPT the 8-byte generation slot
///   3. store generation = prev_even + 2 (even, Release)
/// The generation field is owned by the atomic store, never by the memcpy.
pub fn publish(shared: &mut [u8], image: &[u8]) -> Result<(), PublishError>;

/// Read a shared buffer, retrying if a publish overlaps:
///   loop { g1 = gen.load(Acquire); if g1 is odd { spin; continue }
///          r = f(&SnapshotReader::open(shared)?);
///          g2 = gen.load(Acquire); if g1 == g2 { return r } }
/// `f` must be side-effect-free (it may run on torn data before a retry).
pub fn read_stable<T>(shared: &[u8], f: impl Fn(&SnapshotReader) -> T) -> T;
```

For static/non-shared buffers (unit tests), `SnapshotReader::open` + direct
queries suffice — no seqlock needed, and **no alignment requirement** (all field
reads use `from_le_bytes` over slices).

**Alignment requirement — seqlock path only.** `publish`/`read_stable` access the
generation as an `AtomicU64`, which requires the generation field (Header offset
8) to be 8-byte aligned. Real shared-memory mappings are page-aligned, so this
always holds in production. Tests that exercise the seqlock must use an
8-aligned buffer; `vfs-shared` provides a small `aligned_buffer(len) -> AlignedVec`
test/helper for that. `publish`/`read_stable` validate the buffer's alignment and
return `PublishError::Misaligned` / treat it as a precondition (the plan pins the
exact mechanism — e.g. `AtomicU64::from_mut` over the aligned generation slot, or
a safe atomic-over-bytes helper; still no hand-written `unsafe` in the crate).

---

## 6. Fold & consistency contracts, forward notes

**Fold contract.** The snapshot stores folded child names; the reader compares
them against caller-supplied folded keys with **raw byte equality**. Therefore
**the builder's folding and the reader-caller's folding must be the same
function.** This slice:
- The `bridge` (server side) folds with `vfs-core`'s fold when flattening.
- The round-trip test folds via `vfs-core`.
- `vfs-shared` itself contains **no** fold — it is oblivious, comparing bytes.

**Forward note (shim slice):** when the shim is built it will need the same fold
to turn NT paths into snapshot keys. At that point, extract `vfs-core`'s
`fold`/`cmp_ci` (and likely `normalize_vpath`/`wildcard_match`) into a small leaf
`vfs-path` crate shared by `vfs-core`, the server, and the shim — the single
canonical fold. Not done now (the shim doesn't exist yet); the contract is
documented so the extraction is a clean move, not a correctness fix.

**Consistency model.** Single reused buffer + seqlock. The writer builds a
complete immutable image privately (`SnapshotBuilder::finish`), then `publish`es
it in one short window (odd → copy → even). Readers `read_stable` and retry
across an overlapping publish. Because the buffer is reused in place, there is no
separate memory to reclaim — reclamation/double-buffering (needed only for
future large in-place mutation via RCU) is out of scope.

**Bitness neutrality (G9/H6/D0).** Fixed-width fields, `u32` indices, absolute
`u32` offsets, no `usize`/pointers. Compile-time layout asserts (§3) run now
under x64 and, in CI later, under `i686` — even though the x86 shim ships
post-MVP — so the wire format cannot drift out of x86-readiness.

---

## 7. Error handling & testing

### Error types

```rust
pub enum LayoutError { TooSmall, BadMagic, BadVersion, RegionOutOfBounds, BadRoot }
pub enum ReadError   { NotADirectory, NotFound }
pub enum PublishError { ImageTooLarge, BadImage, Misaligned }
```

`getattr` returns `Option` (absence is expected); `resolve` returns
`SnapResolution::NotFound`; `readdir` returns `ReadError`. No panics on any input
or on torn data.

### Testing strategy

- **Compile-time layout asserts** (§3) — size/alignment of `Header`, `SnapNode`,
  `SnapChild`; a note that CI compiles them under `i686` too (D0/G9).
- **Builder↔reader round-trip (no `vfs-core`):** build snapshots from fixtures
  (nested dirs, files with metadata/source/cache_key), assert
  `getattr`/`resolve`/`readdir` return the expected values; `readdir` order is
  case-insensitive (already-sorted child runs); binary-search lookup hits and
  misses.
- **Torn-read / robustness:** feed truncated buffers, bad magic/version,
  out-of-bounds offsets, `child_first`/`node` past the arrays; assert
  `open`/queries return errors and **never panic** (table-driven; a fuzz-style
  loop mutating a valid image byte-by-byte and asserting no panic).
- **Seqlock deterministic tests:** `publish` leaves generation even and content
  readable; `read_stable` over a stable buffer returns `f`'s result; manually
  setting the generation odd makes `read_stable` spin until it's evened
  (bounded/mocked to avoid an infinite loop in the test).
- **Seqlock concurrency test** (integration crate, may use `unsafe` for a
  `*mut u8` shared across threads since the crate itself forbids it): one writer
  thread republishing two alternating snapshots in a loop while N reader threads
  `read_stable`; assert every read observes a *self-consistent* snapshot (opens
  OK, root in bounds, a known invariant holds) — never a torn mix.
- **Round-trip vs `vfs-core`** (feature `bridge`, `vfs-core` dev-dependency):
  build a `VfsTree`, `flatten` it, and assert the `SnapshotReader`'s
  `getattr`/`resolve`/`readdir` answers **match `vfs-core`'s own answers** across
  a path set — proving the layout faithfully preserves the merged view.

### The `vfs-core` walk API (additive)

`bridge::flatten` needs to read `vfs-core`'s tree. `vfs-core` today exposes no
node-walk. This slice adds a **small, additive, read-only** traversal to
`vfs-core` (e.g., `VfsTree::walk_postorder(&self, visit)` yielding each node's
kind, display name, folded name, metadata, source, cache_key, and children) —
no behavior change to existing APIs. The plan pins the exact signature.

---

## 8. Dependencies & toolchain

- **Toolchain:** stable Rust.
- **Default build:** no dependencies. `#![forbid(unsafe_code)]`.
- **Feature `bridge`:** adds `vfs-core` (path dependency).
- **Dev-dependencies:** `vfs-core` (for the round-trip test under `bridge`).
- **Workspace:** `crates/vfs-shared` added to the workspace `members`.

---

## 9. Out-of-scope reminders (keep the slice tight)

- No cache index, no refcounts.
- No ring/arena/sync regions.
- No OS shared-memory mapping.
- No reclamation/double-buffering/RCU.
- No fold logic inside `vfs-shared`.
- No runtime mutation — the snapshot is an already-merged, immutable tree.

*End of spec.*
