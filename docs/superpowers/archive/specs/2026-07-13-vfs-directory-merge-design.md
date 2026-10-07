# Directory Merge Transform — Design Spec

**Status:** Approved-to-proceed (read-path hook program, Slice C), ready for planning.
**Date:** 2026-07-13
**Slice:** Read-path hook program, **Slice C** — the pure `merge_directory`
transform: given a directory's real on-disk entries plus the snapshot's virtual
children, produce the merged listing a target should see (overrides win,
tombstones hidden, wildcard-filtered, case-insensitively ordered). Feeds the
stateful `NtQueryDirectoryFile[Ex]` hook (Slice E).
**Parent docs:** hook-surface plan (memory: *vfs-hook-surface-plan*).
**Depends on:** Slice A (snapshot tombstones + `SnapshotReader::readdir` listing
them), Slice B (`under_root`/`locate` in `vfs-redirect`).

---

## 1. Context

Directory enumeration is the one read operation that must *combine* two sources:
the real directory on disk (which the hook reads via the original NT call) and
the snapshot's virtual children for that directory. The merge rules — mod files
override same-named real files, mod-added files appear, tombstoned names vanish,
all deduped case-insensitively and filtered by the caller's wildcard — are pure
logic. This slice isolates them as a fully unit-tested transform so the Slice-E
hook only has to marshal NT buffers and hold per-handle enumeration state.

---

## 2. Scope & boundary

Only `crates/vfs-redirect`. Pure, `#![forbid(unsafe_code)]`. No hook, no per-handle
state (that's Slice E), no info-class buffer marshalling.

### In scope

- A public `DirItem { name: String, is_dir: bool, size: u64, mtime: i64 }` used
  for both the caller's real entries and the merged output.
- `RootMap::merge_directory(dir_nt_path, snap, real: &[DirItem], wildcard:
  Option<&str>) -> Vec<DirItem>`.
- A small refactor: factor `under_root(nt_path) -> Option<Vec<String>>` (folded
  remainder components if under root, else `None`) out of Slice B's `locate`, and
  reuse it here and in `locate`.

### Out of scope

- The `NtQueryDirectoryFile[Ex]` hook, per-handle resume/restart/single-entry
  state, the various `FileXxxDirectoryInformation` struct layouts (Slice E).
- Enumerating the real directory (the hook does that via the trampoline).
- V-dir-add *open* semantics (Slice D). Identity, writes.

---

## 3. Merge algorithm

Keyed by folded name (case-insensitive), so duplicates collapse and iteration is
case-insensitively ordered:

1. Seed a `BTreeMap<String /*folded*/, DirItem>` with every `real` entry.
2. If `under_root(dir_nt_path)` is `Some(folded_comps)` and
   `snap.readdir(&folded_comps)` succeeds (the dir exists virtually), overlay each
   virtual child by folded name:
   - `Tombstone` → **remove** that key (hide a real or lower entry).
   - `File` → insert/replace with `{ is_dir: false, size, mtime }` (mod wins).
   - `Dir` → insert/replace with `{ is_dir: true, size: 0, mtime: 0 }`.
   (If the dir is out of root, or absent from the snapshot, there is no overlay —
   the result is just the real entries, filtered.)
3. Filter by `wildcard` (matched against each entry's display `name` via
   `vfs_core::wildcard_match`; `None` ⇒ keep all).
4. Return `map.into_values()` (already ascending by folded key ⇒ case-insensitive
   order). No separate sort needed.

The display `name` of an overridden entry is the **virtual** one (the mod's
casing), since the virtual entry replaces the real one in the map.

---

## 4. API

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirItem {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

impl RootMap {
    pub fn merge_directory(
        &self,
        dir_nt_path: &str,
        snap: &SnapshotReader,
        real: &[DirItem],
        wildcard: Option<&str>,
    ) -> Vec<DirItem>;
}
```

---

## 5. Error handling

Total, no panics. A dir not under the root or not present in the snapshot simply
gets no overlay (returns the filtered real entries). `snap.readdir` errors
(`NotFound`/`NotADirectory`) are treated as "no virtual overlay".

## 6. Testing (pure, exhaustive)

Build a snapshot (via `vfs-core` + `bridge::flatten`) whose `data` dir has a
virtual file `Mod.esp`, a virtual override `Shared.esp`, a virtual subdir
`AddedDir`, and a tombstone `Deleted.esp`. Provide `real` entries and assert:

- **Real-only dir (not in snapshot):** merged == real, filtered + ordered.
- **Override:** real `Shared.esp`(size 1) + virtual `Shared.esp`(size 99) → one
  entry, size 99 (mod wins).
- **Add:** virtual `Mod.esp`/`AddedDir` not in real → present in output.
- **Tombstone hides:** real `Deleted.esp` → omitted from output.
- **Case-insensitive override/dedupe:** real `SHARED.ESP` + virtual `Shared.esp`
  → one entry, display name `Shared.esp`.
- **Wildcard filter:** `Some("*.esp")` drops `AddedDir` and any non-`.esp`.
- **Ordering:** mixed-case names come out case-insensitively ascending.
- **Out-of-root dir:** no overlay; returns filtered real entries unchanged.

## 7. Dependencies & toolchain

Stable. `#![forbid(unsafe_code)]`. Uses `vfs_core::{fold, wildcard_match}` and
`vfs_shared::{SnapshotReader, NodeKind}`. Dev-deps unchanged.

## 8. Out-of-scope reminders

No hook, no per-handle state, no NT info-class structs, no writes, no identity.

*End of spec.*
