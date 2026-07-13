# First-Class Tombstones — Design Spec

**Status:** Approved-to-proceed (standing goal + user-directed read-path hook
program, Slice A), ready for planning.
**Date:** 2026-07-13
**Slice:** Read-path hook program, **Slice A** — make a mod's deletion of a file
survive as a first-class **tombstone (whiteout)** node all the way into the
snapshot, so the shim can *hide a real on-disk game file* instead of passing the
open through to it. Prerequisite for every read-path transform's tombstone
behavior.
**Parent docs:** *VFS Design* (whiteout semantics), the hook-surface plan
(memory: *vfs-hook-surface-plan*).
**Depends on:** existing `vfs-core` tree + `vfs-shared` snapshot.

---

## 1. Context & problem

`vfs-core::build` currently applies `EntryKind::Tombstone` by **removing** the
node from the tree (`tree.rs` `remove_path`). Consequence: a net-deleted path is
simply *absent* from the tree and the snapshot, indistinguishable from a path no
mod ever mentioned. When the shim opens such a path, the snapshot resolves
`NotFound` → the shim passes through → **the real game file the mod deleted gets
opened anyway.** Deletions therefore do not hide real files.

Fix: a tombstone must persist as an explicit **deny** node in the tree and the
snapshot. Resolving a tombstoned path yields a distinct `Tombstone` result the
shim maps to `STATUS_OBJECT_NAME_NOT_FOUND`; directory listings expose tombstone
children so the merge transform (Slice C) can subtract the denied names from the
real directory's entries.

---

## 2. Scope & boundary

Touches `vfs-core` (tree, model, walk) and `vfs-shared` (layout, builder, bridge,
reader). No new crates. Read-only semantics only (tombstones are produced by the
input layers exactly as today via `EntryKind::Tombstone`; this slice changes only
how they are *retained and surfaced*, not how they're authored).

### In scope

- `vfs-core`: a `NodeEntry::Tombstone` leaf; `build` inserts/replaces it (creating
  parent dirs) instead of deleting; last-write-wins across layers still holds
  (a higher layer's File/Dir replaces a tombstone and vice-versa). `resolve` →
  `Resolution::Tombstone`; `getattr` → `None` (a tombstoned path has no stat);
  `readdir` includes tombstone children (kind `Tombstone`); `walk_postorder`
  emits `WalkNodeKind::Tombstone`.
- `vfs-shared`: `KIND_TOMBSTONE = 2` (a new value in the existing 1-byte `kind`
  field — no struct-size/offset change, asserts unaffected); `builder` gains
  `add_tombstone`; `bridge` maps `WalkNodeKind::Tombstone`; `reader` recognizes it
  — `SnapResolution::Tombstone`, `getattr` → `None`, `readdir` includes tombstone
  entries; `SnapshotReader::node_kind` → `NodeKind::Tombstone`.

### Out of scope

- The read-path transforms that *consume* tombstones (Slices B/C) and the shim
  hooks (P2). Runtime authoring of tombstones / write path (P3). Any change to how
  layers are supplied.

---

## 3. Semantics

- **`Resolution`/`SnapResolution`** gain a `Tombstone` variant (peer of `File`,
  `Dir`, `NotFound`). Meaning: "this path is explicitly deleted — deny it, do NOT
  fall through to a real file."
- **Layering:** processing layers in order, `Tombstone` replaces whatever a lower
  layer put at that path; a later `File`/`Dir` at the same path replaces the
  tombstone. The *final* tree state at each path is what the snapshot records.
- **Parents:** inserting a tombstone `ensure_dir_path`s its parents (so the deny
  is reachable). Those parent dirs become virtual dirs that merge with the real
  dir (correct — the real directory still exists; only the one child is hidden).
- **`getattr`** returns `None` for a tombstone (no file/dir attributes). The
  attribute-query transform (Slice B) will use `resolve` to distinguish
  `Tombstone` (deny, return not-found) from `NotFound` (pass through).
- **`readdir`/`SnapshotReader::readdir`** include tombstone children as entries
  with kind `Tombstone`, so the merge transform can remove those names from the
  real listing. `NodeKind` gains a `Tombstone` variant used only in directory
  entries (never in a `Stat`, since `getattr` returns `None` for tombstones).

---

## 4. API deltas

```rust
// vfs-core::model
pub enum Resolution { File{..}, Dir, Tombstone, NotFound }   // + Tombstone
pub enum NodeKind   { Dir, File, Tombstone }                 // + Tombstone (dir-entry use)

// vfs-core::tree (WalkNodeKind)
pub enum WalkNodeKind<'a> { Dir, File{..}, Tombstone }       // + Tombstone

// vfs-shared::layout
pub const KIND_TOMBSTONE: u8 = 2;

// vfs-shared::reader
pub enum NodeKind      { Dir, File, Tombstone }              // + Tombstone
pub enum SnapResolution{ File{..}, Dir, Tombstone, NotFound }// + Tombstone
// SnapshotReader::getattr -> None for tombstone; readdir includes Tombstone entries.

// vfs-shared::builder
impl SnapshotBuilder { pub fn add_tombstone(&mut self, name: &str, folded: &str) -> u32; }
```

Existing variant match sites (in both crates and any tests) must add the
`Tombstone` arm; non-exhaustive matches on these enums will fail to compile until
updated — intentional, so nothing silently mishandles a tombstone.

---

## 5. Error handling

No new fallibility. `getattr` already returns `Option`; `resolve` is total. The
reader keeps its bounds-checked, torn-read-safe discipline: an unknown kind byte
(neither 0/1/2) still yields `None`/`NotFound` (never panics).

## 6. Testing

- **vfs-core:** tombstone over a lower-layer file → `resolve == Tombstone`;
  tombstone with no lower entry → `Tombstone` (records the deny); `File` in a
  higher layer over a tombstone → `File`; a tombstone in a higher layer over a
  `File` → `Tombstone`; parents of a tombstone are created; `getattr(tombstone)
  == None`; `readdir(parent)` contains the tombstoned name with kind `Tombstone`.
- **vfs-shared:** `flatten` a tree containing a tombstone, open with
  `SnapshotReader`: `resolve == SnapResolution::Tombstone`, `getattr == None`,
  `readdir` lists the tombstone entry; matches `vfs-core` (end-to-end parity).
  Builder unit test: a node written with `add_tombstone` reads back as
  `NodeKind::Tombstone`.

## 7. Dependencies & toolchain

Stable. `vfs-core` stays `#![forbid(unsafe_code)]`; `vfs-shared` keeps its single
audited unsafe (unchanged). No new deps. No snapshot size/offset change (only a
new value in the existing `kind` byte).

## 8. Out-of-scope reminders

No transforms, no hooks, no write path, no layer-authoring changes.

*End of spec.*
