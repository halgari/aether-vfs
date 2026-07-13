# Directory Merge Transform Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A pure `RootMap::merge_directory` that combines a directory's real on-disk entries with the snapshot's virtual children — overrides win, tombstones vanish, wildcard-filtered, case-insensitively ordered — exhaustively unit-tested.

**Architecture:** Factor `under_root(nt_path) -> Option<Vec<String>>` out of Slice B's `locate` (reuse in both). `merge_directory` seeds a `BTreeMap<folded_name, DirItem>` from the real entries, overlays the snapshot's `readdir` children (tombstone removes, file/dir insert-or-replace), filters by wildcard, and returns the map's values (already case-insensitively ordered by folded key).

**Tech Stack:** Rust (stable). `vfs-redirect`; uses `vfs_core::{fold, wildcard_match}`, `vfs_shared::{SnapshotReader, NodeKind}`; dev-deps `vfs-core` + `vfs-shared` (`bridge`).

## Global Constraints

- Stable; `#![forbid(unsafe_code)]` stays.
- `DirItem` derives `Debug, Clone, PartialEq, Eq` (asserted via `assert_eq!`).
- Total & fail-safe: a dir out of root or absent from the snapshot gets no overlay (returns filtered real entries); `snap.readdir` errors ⇒ no overlay. No panics.
- Merge is case-insensitive: keyed by `fold(name)`; overridden entries take the virtual (mod) display name; output ordered ascending by folded key.

---

### Task 1: `under_root` refactor + `DirItem` + `merge_directory`

**Files:**
- Modify: `crates/vfs-redirect/src/lib.rs`
- Test: `crates/vfs-redirect/src/lib.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: Slice B's `Located`/`locate`; `vfs_shared::{SnapshotReader, NodeKind}`; `vfs_core::{fold, wildcard_match}`.
- Produces: `DirItem`, `RootMap::merge_directory`, and a private `RootMap::under_root`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block of `crates/vfs-redirect/src/lib.rs`. A
dedicated fixture builds a `data` dir with a virtual file, an override, an added
subdir, and a tombstone:

```rust
    // data/ has: Mod.esp (add), Shared.esp (override, size 99), AddedDir (dir),
    // Deleted.esp (tombstone).
    fn merge_snapshot() -> Vec<u8> {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let mk = |vpath: &str, kind: EntryKind, size: u64| InputEntry {
            vpath: vpath.into(),
            kind,
            source: r"D:\Mods\X\f".into(),
            size,
            mtime: 7,
        };
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![
                mk("data/Mod.esp", EntryKind::File, 5),
                mk("data/Shared.esp", EntryKind::File, 99),
                mk("data/AddedDir", EntryKind::Dir, 0),
                mk("data/Deleted.esp", EntryKind::Tombstone, 0),
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    }

    fn item(name: &str, is_dir: bool, size: u64) -> DirItem {
        DirItem { name: name.into(), is_dir, size, mtime: 0 }
    }

    fn names(v: &[DirItem]) -> Vec<String> {
        v.iter().map(|e| e.name.clone()).collect()
    }

    const DATA_NT: &str = r"\??\C:\Games\Skyrim\Data";

    #[test]
    fn merge_overrides_adds_and_hides_tombstones() {
        let bytes = merge_snapshot();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let real = vec![
            item("Shared.esp", false, 1), // overridden by virtual (size 99)
            item("Deleted.esp", false, 1), // tombstoned away
            item("RealOnly.txt", false, 7), // survives
        ];
        let merged = root().merge_directory(DATA_NT, &snap, &real, None);
        // Case-insensitive folded order: addeddir, mod.esp, realonly.txt, shared.esp
        assert_eq!(names(&merged), vec!["AddedDir", "Mod.esp", "RealOnly.txt", "Shared.esp"]);
        let shared = merged.iter().find(|e| e.name == "Shared.esp").unwrap();
        assert_eq!(shared.size, 99); // mod wins
        let added = merged.iter().find(|e| e.name == "AddedDir").unwrap();
        assert!(added.is_dir);
        assert!(!merged.iter().any(|e| e.name == "Deleted.esp"));
    }

    #[test]
    fn merge_is_case_insensitive_override() {
        let bytes = merge_snapshot();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let real = vec![item("SHARED.ESP", false, 1)];
        let merged = root().merge_directory(DATA_NT, &snap, &real, None);
        // One entry, display name from the virtual (mod) side.
        let shared: Vec<&DirItem> = merged.iter().filter(|e| e.name.eq_ignore_ascii_case("shared.esp")).collect();
        assert_eq!(shared.len(), 1);
        assert_eq!(shared[0].name, "Shared.esp");
        assert_eq!(shared[0].size, 99);
    }

    #[test]
    fn merge_wildcard_filters_output() {
        let bytes = merge_snapshot();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let real = vec![item("RealOnly.txt", false, 7)];
        let merged = root().merge_directory(DATA_NT, &snap, &real, Some("*.esp"));
        // AddedDir and RealOnly.txt filtered out; only *.esp remain.
        assert_eq!(names(&merged), vec!["Mod.esp", "Shared.esp"]);
    }

    #[test]
    fn merge_out_of_root_returns_filtered_real() {
        let bytes = merge_snapshot();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let real = vec![item("a.dll", false, 1), item("b.exe", false, 2)];
        let merged = root().merge_directory(r"\??\C:\Windows\System32", &snap, &real, Some("*.dll"));
        assert_eq!(names(&merged), vec!["a.dll"]);
    }

    #[test]
    fn merge_real_only_dir_not_in_snapshot() {
        let bytes = merge_snapshot();
        let snap = SnapshotReader::open(&bytes).unwrap();
        // `data/sub` is under root but not in the snapshot -> no overlay.
        let real = vec![item("z.txt", false, 1), item("a.txt", false, 2)];
        let merged = root().merge_directory(r"\??\C:\Games\Skyrim\Data\sub", &snap, &real, None);
        assert_eq!(names(&merged), vec!["a.txt", "z.txt"]); // ordered, no overlay
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-redirect merge_`
Expected: FAIL to compile (`DirItem`/`merge_directory` undefined).

- [ ] **Step 3: Implement `DirItem`, `under_root`, `merge_directory`; refactor `locate`**

(a) Update the imports at the top of `crates/vfs-redirect/src/lib.rs`:

```rust
use std::collections::BTreeMap;

use vfs_core::{fold, normalize_vpath, wildcard_match, PathError};
use vfs_shared::{NodeKind, SnapResolution, SnapshotReader};
```

(b) Add the public type (near `Decision`/`AttrDecision`):

```rust
/// One entry in a directory listing — used both for the caller's real on-disk
/// entries and for the merged result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirItem {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}
```

(c) In `impl RootMap`, add `under_root` and refactor `locate` to use it. Replace
the existing `locate` body:

```rust
    /// Folded remainder components if `nt_path` is under the managed root, else
    /// `None` (out of root, malformed, or escaping).
    fn under_root(&self, nt_path: &str) -> Option<Vec<String>> {
        let norm = normalize_vpath(nt_path).ok()?;
        let comps: Vec<&str> =
            if norm.is_empty() { Vec::new() } else { norm.split('/').collect() };
        if comps.len() < self.root.len() {
            return None;
        }
        for (r, c) in self.root.iter().zip(comps.iter()) {
            if fold(r) != fold(c) {
                return None;
            }
        }
        Some(comps[self.root.len()..].iter().map(|c| fold(c)).collect())
    }

    fn locate(&self, nt_path: &str, snap: &SnapshotReader) -> Located {
        match self.under_root(nt_path) {
            None => Located::Outside,
            Some(folded) => {
                let refs: Vec<&str> = folded.iter().map(String::as_str).collect();
                Located::Resolved(snap.resolve(&refs))
            }
        }
    }
```

(d) Add `merge_directory` to `impl RootMap`:

```rust
    /// Merge a directory's real on-disk `real` entries with the snapshot's
    /// virtual children: overrides win, tombstones are hidden, `wildcard` filters
    /// the display names, output is case-insensitively ordered by folded name.
    pub fn merge_directory(
        &self,
        dir_nt_path: &str,
        snap: &SnapshotReader,
        real: &[DirItem],
        wildcard: Option<&str>,
    ) -> Vec<DirItem> {
        let mut map: BTreeMap<String, DirItem> = BTreeMap::new();
        for e in real {
            map.insert(fold(&e.name), e.clone());
        }
        if let Some(folded) = self.under_root(dir_nt_path) {
            let refs: Vec<&str> = folded.iter().map(String::as_str).collect();
            if let Ok(virt) = snap.readdir(&refs) {
                for v in virt {
                    let key = fold(&v.name);
                    match v.kind {
                        NodeKind::Tombstone => {
                            map.remove(&key);
                        }
                        NodeKind::Dir => {
                            map.insert(key, DirItem { name: v.name, is_dir: true, size: 0, mtime: 0 });
                        }
                        NodeKind::File => {
                            map.insert(
                                key,
                                DirItem { name: v.name, is_dir: false, size: v.size, mtime: v.mtime },
                            );
                        }
                    }
                }
            }
        }
        map.into_values()
            .filter(|e| match wildcard {
                Some(p) => wildcard_match(p, &e.name),
                None => true,
            })
            .collect()
    }
```

- [ ] **Step 4: Run to verify passing**

Run: `cargo test -p vfs-redirect`
Expected: PASS — the 5 new merge tests plus all existing `decide`/`query_attributes`
tests (the `locate` refactor is behavior-preserving).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-redirect/src/lib.rs
git commit -m "vfs-redirect: merge_directory transform (override/add/tombstone/wildcard/order)"
```

---

### Task 2: Verification sweep

**Files:** none.

- [ ] **Step 1: Full workspace test**

Run: `cargo test --workspace`
Expected: all green (no downstream match sites consume `DirItem`).

- [ ] **Step 2: Warning check**

Run: `cargo build --workspace 2>&1 | rg -i "warning" || echo "no warnings"`
Expected: `no warnings`.

- [ ] **Step 3: Commit Cargo.lock (if changed)**

```bash
git add Cargo.lock
git commit -m "directory-merge: update Cargo.lock" || echo "nothing to commit"
```

---

## Self-Review Notes

- **Spec coverage:** `DirItem` + `merge_directory` (Task 1); every §6 matrix row is a test (override, add, tombstone-hide, case-insensitive dedupe, wildcard filter, ordering, real-only, out-of-root). The `under_root` refactor keeps `locate`/`decide`/`query_attributes` behavior identical.
- **Derives:** `DirItem` derives `Debug, Clone, PartialEq, Eq` (used in `assert_eq!` via the `names()` helper and direct field asserts).
- **Ordering:** `BTreeMap<fold(name), _>` yields ascending-by-folded iteration ⇒ case-insensitive order with no explicit sort. Override uses the virtual display name because the virtual insert replaces the real value under the same folded key.
- **No `.unwrap_err()` hazards:** tests use `assert_eq!`/`assert!` on `Vec<DirItem>`/bools only.
