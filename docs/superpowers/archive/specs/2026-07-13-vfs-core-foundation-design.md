# vfs-core Foundation — Design Spec

**Status:** Approved (design), ready for implementation planning
**Date:** 2026-07-13
**Slice:** First implementable slice of the Usermode VFS project — the pure,
OS-independent provider core (read-only merged view).
**Parent docs:** *Usermode VFS for Windows — Design Document*, *Out-of-Process
(IPC) Architecture*, *Rust Implementation Guide & API Surface*.

---

## 1. Context & positioning

The overall project is a process-scoped usermode VFS for Windows game modding
(a USVFS successor). Its architecture is fully specified across three parent
documents: interception constraints (C1–C3), an out-of-process FUSE-like design
with a recursion-free shared-memory transport, ~8 Rust crates, 13 guards
(G1–G13), and a milestone plan (M0–M7).

This spec covers **only the first slice**: the `vfs-core` crate — the
OS-independent "portable heart" that resolves a merged/overlaid virtual
namespace. Per the Rust Implementation Guide (§3), `vfs-core` must not depend on
`windows*`/`ntapi` and ultimately runs **in the server** behind the IPC
boundary. This slice builds the read-only resolution logic and nothing else.

Every subsequent slice (IPC substrate, shim, injection, etc.) is a separate
spec → plan → implementation cycle.

### Decisions locked during brainstorming

1. **Start with `vfs-core`** — the dependency root; pure logic; fully unit
   testable; directly satisfies acceptance §8.1.
2. **In-memory layers (data-in)** — the caller enumerates every source layer
   (mod dirs *and* a snapshot of the real game dir) and hands `vfs-core` a list
   of layers as `(relative-path, metadata)` entries. Core does **zero I/O**; it
   is a pure function of its inputs.
3. **Read-only merged view** — build + `getattr`/`readdir`/`resolve`, honoring
   priority order and pre-declared tombstones. **No runtime mutation**
   (writes/renames/deletes) in this slice — that is stateful, ties into the
   overwrite-dir + server, and gets its own later slice.
4. **Approach A — idiomatic Rust tree; arena serialization deferred.** Core
   builds a natural Rust structure optimized for clarity/correctness. The
   bitness-neutral shared-memory arena layout is a separate concern in
   `vfs-shared` (built during M0). Core will later gain a small
   `flatten_to_arena()` bridge; it is **not** built now.
5. **UTF-8 `String` for names** — idiomatic, testable, portable. The shim /
   director convert UTF-16↔UTF-8 at the edges. The only thing this cannot
   losslessly represent is ill-formed UTF-16 (unpaired surrogates), which does
   not occur for real game asset filenames. `widestring`/`Vec<u16>` is the
   documented fallback if that assumption is ever violated.

---

## 2. Scope & crate boundary

`vfs-core` is a standalone library crate: **no `windows`/`ntapi`, no `unsafe`,
no filesystem I/O**, buildable on **stable** Rust. It is a pure function of its
inputs.

### In scope (this slice)

- `build(layers) → VfsTree` — fold ordered layers into one merged, queryable
  tree.
- `getattr(vpath)` — metadata for a virtual node.
- `resolve(vpath)` — vpath → winning source file / directory / not-found.
- `readdir(vpath, filter)` — merged, case-insensitively sorted listing, honoring
  tombstones, with an optional wildcard filter.
- Path normalization (`.`/`..`, separator folding, case-fold, known NT/DOS
  prefix stripping).
- DOS wildcard matching (`* ? < > "` semantics, case-insensitive).
- Cache-key computation for a resolved file.

### Explicitly out of scope (later slices)

- Runtime mutation: writes/renames/deletes, overwrite-dir, dynamic tombstone
  creation.
- The shared-memory arena / bitness-neutral layout — that is `vfs-shared`.
- Actual byte materialization, delete-on-close backing files, handle
  duplication — that is the server.
- Image-vs-data classification — that is shim policy (parent design §5.3).
- Any real filesystem I/O — the director feeds enumerated data in.

### The one bridge we design *for* but don't build

A future `flatten_to_arena()` will walk `VfsTree` into `vfs-shared`'s
`u32`-indexed form. The tree is therefore shaped so nodes are addressable by a
`u32` index and children are enumerable in sorted order, making that later step
mechanical. We do **not** implement it in this slice.

---

## 3. Data model

### Inputs

A `Layer` is one overlay source. The caller passes layers **low→high priority**:
`layers[0]` is the real game dir (lowest), followed by mods; the **highest index
wins**. Each layer is a flat set of entries:

```rust
pub struct LayerId(pub u32);
pub struct Layer { pub id: LayerId, pub entries: Vec<InputEntry> }

pub enum EntryKind { File, Dir, Tombstone }   // Tombstone = whiteout / virtual-delete

pub struct InputEntry {
    pub vpath:  String,      // root-relative virtual path
    pub kind:   EntryKind,
    pub source: SourceId,    // opaque token; only meaningful for File
    pub size:   u64,         // File only
    pub mtime:  i64,         // File only; opaque, used for getattr + cache key
}

pub struct SourceId(pub Box<[u8]>);   // opaque to core; stored, returned, hashed verbatim
```

`SourceId` is **opaque to core** — whatever the director needs to later locate
the real bytes (a real path, a handle id, etc.). Core never interprets it; it
only stores, returns, and hashes it.

### Merged tree

Nodes live in a `Vec<Node>` addressed by a `u32` `NodeId`; directories hold
`children: BTreeMap<FoldedName, NodeId>` (a case-folded key → child index). This
shape is deliberately arena-friendly for the future `flatten_to_arena()` bridge.

`build` folds layers low→high:

- A **File** entry sets/replaces the winner at its path.
- A **Tombstone** removes whatever node is currently at that path (hiding all
  lower layers); a later higher layer can resurrect the path.
- **Directories union** across layers.
- On a **file-vs-dir conflict** at one path, **higher priority wins**, replacing
  the lower node wholesale.

Display names preserve the **winning** layer's original casing (case-preserving);
all lookups are case-insensitive.

---

## 4. Public API surface

```rust
// ---- Build ----
pub fn build(layers: Vec<Layer>) -> Result<VfsTree, BuildError>;

// ---- Query (root-relative virtual paths; normalized internally) ----
impl VfsTree {
    pub fn getattr(&self, vpath: &str) -> Option<Stat>;
    pub fn resolve(&self, vpath: &str) -> Resolution;
    pub fn readdir(&self, vpath: &str, filter: Option<&str>)
        -> Result<Vec<DirEntry>, VfsError>;
}

pub enum NodeKind { File, Dir }
pub struct Stat     { pub kind: NodeKind, pub size: u64, pub mtime: i64 }
pub struct DirEntry { pub name: String, pub kind: NodeKind, pub size: u64, pub mtime: i64 }

pub enum Resolution {
    File { source: SourceId, size: u64, mtime: i64, layer: LayerId, cache_key: CacheKey },
    Dir,
    NotFound,   // absent OR tombstoned — indistinguishable by design
}

pub struct CacheKey(pub [u8; 32]);   // blake3 of (source bytes ‖ size ‖ mtime)

// ---- Free helpers (edges / tests) ----
pub fn normalize_vpath(raw: &str) -> Result<String, PathError>;  // ./.., separators, strip \??\ \\?\
pub fn wildcard_match(pattern: &str, name: &str) -> bool;        // DOS semantics, case-insensitive
```

- `readdir` returns entries **already case-insensitively sorted**; `filter`
  applies `wildcard_match` per entry.
- Every query takes `&self` (immutable). The tree is built once and queried
  concurrently by many reader threads — mapping cleanly onto the seqlock
  snapshot model later.

---

## 5. Resolution semantics (pinned)

- **Priority:** higher layer index wins for files. Ties impossible (indices
  unique).
- **Directory union:** a dir exists if *any* non-tombstoned layer has it; its
  children are the union across layers, each child independently resolved.
- **Tombstone:** hides the exact path (and, for a directory tombstone, the
  subtree) from all **lower** layers; a higher layer re-adding the path
  resurrects it. Modeled by removing the current node during the low→high fold.
- **File-vs-dir conflict** at one path: higher priority wins, replacing the
  lower node wholesale.
- **Case:** case-insensitive lookup/dedup via a single fold helper; display name
  preserves the winning layer's casing. `readdir` sorts **case-insensitively**
  (explicit regression guard against USVFS's reverse-alphabetical bug).
- **Cache key:** `blake3(source_bytes ‖ size_le ‖ mtime_le)`. Identical resolved
  inputs → identical key (dedupe); changed size/mtime → new key. Computed lazily
  on `resolve`, not at build.
- **NotFound vs tombstoned:** collapsed into one `NotFound` — callers cannot
  distinguish, which is the correct virtual-delete behavior.

### Case folding

A single internal fold helper is the source of truth for both lookup keys and
sort order. MVP uses Unicode simple case folding for the `BTreeMap<FoldedName,…>`
key and a case-insensitive comparator for `readdir` sort. This helper is
regression-tested (parent design §8.1) and is the *only* place casing is
compared.

### Path normalization

`normalize_vpath` accepts a root-relative virtual path and:
- folds `/` and `\` separators to a single canonical separator,
- resolves `.` and `..` (rejecting `..` that escapes the root → `PathError`),
- strips known NT/DOS prefixes (`\??\`, `\\?\`) if present,
- collapses redundant separators.

The tree is keyed on normalized, root-relative paths. Deeper prefix handling
(`\Device\…`, `RootDirectory`-relative NT opens, 8.3 short-name generation) is an
**edge / shim concern** and out of scope for core; core assumes a normalized
root-relative path on input and provides `normalize_vpath` as the shared helper
the edges call.

---

## 6. Error handling

No panics; no `unwrap` on input-derived data. Small explicit error enums:

- `BuildError` — malformed input `vpath` (empty, absolute where relative
  expected, `..` escaping root).
- `PathError` — from `normalize_vpath` (same causes).
- `VfsError` — `readdir` on a file or on a nonexistent/tombstoned path:
  `NotADirectory`, `NotFound`.

`getattr`/`resolve` use `Option`/`Resolution` for the not-found case (expected,
not exceptional) rather than returning errors.

---

## 7. Testing strategy (maps to acceptance §8.1)

Table-driven unit tests, one module per concern:

- **Merge/overlay:** priority ordering, directory union, file/dir conflicts,
  deep trees, collisions.
- **Tombstones:** whiteout hides lower layers; higher layer resurrects;
  directory-tombstone hides subtree.
- **Case-insensitive sort:** explicit reverse-alphabetical regression fixture.
- **Wildcard parity:** table of `(pattern, name, expected)` against known
  Windows `FsRtlIsNameInExpression` cases, incl. `* ? < > "`.
- **Path normalization:** `.`/`..`, mixed separators, `\??\`/`\\?\` prefixes,
  `..`-escapes-root rejection, Unicode.
- **Cache-key stability:** identical inputs → identical key; changed mtime/size →
  new key; same source+size+mtime under two vpaths → dedupe.

Runtime **refcount transitions** (parent §8.1) are intentionally **not** here —
they belong to the server/mutation slice.

Optional `proptest` for build idempotence and layer-order invariants. Target
near-100% coverage of core, since it is pure and I/O-free.

---

## 8. Dependencies & toolchain

- **Toolchain:** stable Rust (this crate needs no nightly features; nightly is
  only required later for `dll-syringe`/`retour` in the injection/hook crates).
- **Crates:** `blake3` (cache keys). Optionally `proptest` (dev-dependency).
  No `windows`/`ntapi`/`unsafe`.
- **Workspace:** created as `crates/vfs-core` under a Cargo workspace so future
  crates slot in beside it without restructuring.

---

## 9. Out-of-scope reminders (so the slice stays tight)

- No shared memory, no arena, no bitness layout assertions (that is `vfs-shared`
  / M0).
- No hooking, injection, or Windows API surface.
- No mutation, overwrite-dir, or server logic.
- No real filesystem access.

*End of spec.*
