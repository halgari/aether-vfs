# First-Class Tombstones Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A mod's deletion survives as a first-class tombstone node from `vfs-core` layers through the `vfs-shared` snapshot, so a resolver/shim can hide a real on-disk file (`Tombstone`) instead of passing the open through to it (`NotFound`).

**Architecture:** Add a third node kind (`Tombstone`) alongside `File`/`Dir` in the tree, the walk, and the snapshot wire format (a new value `2` in the existing 1-byte `kind` field — no struct-size/offset change). `resolve` returns `Tombstone` (deny) distinct from `NotFound` (absent); `getattr` returns `None`; directory listings include tombstone entries so a later merge transform can subtract denied names.

**Tech Stack:** Rust (stable). `vfs-core` (`#![forbid(unsafe_code)]`), `vfs-shared` (unchanged unsafe surface).

## Global Constraints

- Stable Rust; `vfs-core` stays `#![forbid(unsafe_code)]`; `vfs-shared` keeps its single audited unsafe (do not add unsafe).
- No snapshot struct-size/offset change — only the new `KIND_TOMBSTONE = 2` byte value.
- The new enum variants (`Resolution::Tombstone`, `NodeKind::Tombstone`, `WalkNodeKind::Tombstone`, `SnapResolution::Tombstone`) live on enums that already derive `Debug, Clone, PartialEq, Eq` (or `Copy`) — so `assert_eq!`/`matches!` in tests compile. Do not remove existing derives.
- Every `match` on these enums (incl. in tests) must gain a `Tombstone` arm; a non-exhaustive match is a compile error — intentional (nothing may silently mishandle a tombstone).
- `getattr` returns `None` for a tombstone; `resolve` returns the `Tombstone` variant.

---

### Task 1: `vfs-core` — tombstone node, resolve/getattr/readdir/walk

**Files:**
- Modify: `crates/vfs-core/src/model.rs` (add `Tombstone` to `NodeKind` and `Resolution`)
- Modify: `crates/vfs-core/src/tree.rs` (node variant, build, find/child/ensure, resolve/getattr/readdir/walk)
- Test: `crates/vfs-core/src/tree.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `Resolution::Tombstone`, `NodeKind::Tombstone`, `WalkNodeKind::Tombstone`; `VfsTree::resolve` → `Tombstone` for a net-deleted path; `getattr` → `None`; `readdir` includes tombstone children; `walk_postorder` emits tombstone nodes.

- [ ] **Step 1: Add the model variants**

In `crates/vfs-core/src/model.rs`:
- `NodeKind` becomes `pub enum NodeKind { File, Dir, Tombstone }`.
- `Resolution` gains a `Tombstone` variant (unit) between `Dir` and `NotFound`:

```rust
pub enum Resolution {
    File { source: SourceId, size: u64, mtime: i64, layer: LayerId, cache_key: CacheKey },
    Dir,
    Tombstone,
    NotFound,
}
```

- [ ] **Step 2a: Update the three pre-existing tests that encode the OLD (removal) semantics**

This slice deliberately changes tombstone behavior from "remove the node → `NotFound`, absent from `readdir`" to "first-class `Tombstone` node → `Resolution::Tombstone`, listed in `readdir` as `NodeKind::Tombstone`". Three existing tests in `crates/vfs-core/src/tree.rs` assert the old behavior and MUST be updated (the old behavior was exactly the bug this fixes):

- `tombstone_hides_lower_layer` (~line 369): change the assertion from
  `Resolution::NotFound` to `Resolution::Tombstone`.
- `directory_tombstone_hides_subtree` (~line 393): change the first assertion
  `t.resolve("data/sub")` from `Resolution::NotFound` to `Resolution::Tombstone`;
  KEEP the second assertion `t.resolve("data/sub/a.esp") == Resolution::NotFound`
  (you still can't descend through a tombstone leaf).
- `readdir_honors_tombstones` (~line 469): rename to `readdir_lists_tombstones`
  and change the body's final assertions to:

```rust
        let entries = t.readdir("d", None).unwrap();
        let names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec!["a.esp", "b.esp"]);
        let a = entries.iter().find(|e| e.name == "a.esp").unwrap();
        assert_eq!(a.kind, crate::model::NodeKind::Tombstone);
```

`higher_layer_resurrects_tombstone` (~line 378) still passes unchanged (a re-added
file over a tombstone resolves to `File`).

- [ ] **Step 2b: Add the two genuinely-new tests**

Add to the `#[cfg(test)] mod tests` block (reuse the existing `file`/`tomb`/`layer` helpers):

```rust
    #[test]
    fn tombstone_with_no_lower_entry_still_denies() {
        use crate::model::Resolution;
        let t = build(vec![layer(0, vec![tomb("data/gone.esp")])]).unwrap();
        assert_eq!(t.resolve("data/gone.esp"), Resolution::Tombstone);
    }

    #[test]
    fn tombstone_getattr_is_none_and_parent_lists_it() {
        use crate::model::NodeKind;
        let t = build(vec![layer(0, vec![tomb("data/gone.esp")])]).unwrap();
        assert_eq!(t.getattr("data/gone.esp"), None);
        let entries = t.readdir("data", None).unwrap();
        let e = entries.iter().find(|e| e.name == "gone.esp").unwrap();
        assert_eq!(e.kind, NodeKind::Tombstone);
    }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p vfs-core tombstone`
Expected: FAIL to compile (no `Resolution::Tombstone` / `NodeKind::Tombstone` arms yet; `insert_tombstone` behavior missing).

- [ ] **Step 4: Implement the tombstone node in `tree.rs`**

Make these edits in `crates/vfs-core/src/tree.rs`:

(a) Add the node variant:

```rust
enum NodeEntry {
    File(FileNode),
    Dir(DirNode),
    Tombstone,
}
```

(b) In `build`, replace the tombstone arm:

```rust
                EntryKind::Tombstone => tree.insert_tombstone(&comps),
```

(c) Add `insert_tombstone` next to `insert_file`:

```rust
    /// Insert a tombstone (whiteout) at `comps`, creating parent dirs; replaces
    /// any existing node so the deletion shadows lower layers.
    fn insert_tombstone(&mut self, comps: &[&str]) {
        let (leaf, parents) = comps.split_last().expect("build guarantees non-empty");
        let parent = self.ensure_dir_path(parents);
        let key = fold(leaf);
        match self.child(parent, &key) {
            Some(id) => {
                self.nodes[id as usize].name = leaf.to_string();
                self.nodes[id as usize].entry = NodeEntry::Tombstone;
            }
            None => {
                let id = self.push(leaf, NodeEntry::Tombstone);
                self.set_child(parent, key, id);
            }
        }
    }
```

(d) `find` — cannot descend through a File OR a Tombstone (both are leaves):

```rust
            match &self.nodes[cur as usize].entry {
                NodeEntry::Dir(d) => cur = *d.children.get(&key)?,
                NodeEntry::File(_) | NodeEntry::Tombstone => return None,
            }
```

(e) `child` — a Tombstone has no children:

```rust
    fn child(&self, parent: u32, key: &str) -> Option<u32> {
        match &self.nodes[parent as usize].entry {
            NodeEntry::Dir(d) => d.children.get(key).copied(),
            NodeEntry::File(_) | NodeEntry::Tombstone => None,
        }
    }
```

(f) `ensure_dir_path` — replace ANY non-dir (File or Tombstone) with a dir when a
deeper path needs it:

```rust
                Some(id) => {
                    if !matches!(self.nodes[id as usize].entry, NodeEntry::Dir(_)) {
                        self.nodes[id as usize].name = comp.to_string();
                        self.nodes[id as usize].entry =
                            NodeEntry::Dir(DirNode { children: BTreeMap::new() });
                    }
                    cur = id;
                }
```

(g) `resolve` — add the arm:

```rust
            Some(id) => match &self.nodes[id as usize].entry {
                NodeEntry::Dir(_) => Resolution::Dir,
                NodeEntry::Tombstone => Resolution::Tombstone,
                NodeEntry::File(f) => Resolution::File { /* unchanged */
                    source: f.source.clone(),
                    size: f.size,
                    mtime: f.mtime,
                    layer: f.layer,
                    cache_key: compute_cache_key(&f.source, f.size, f.mtime),
                },
            },
```

(h) `getattr` — rewrite to return `None` for a tombstone:

```rust
    pub fn getattr(&self, vpath: &str) -> Option<crate::model::Stat> {
        use crate::model::{NodeKind, Stat};
        let norm = normalize_vpath(vpath).ok()?;
        let id = self.find(&norm)?;
        match &self.nodes[id as usize].entry {
            NodeEntry::Dir(_) => Some(Stat { kind: NodeKind::Dir, size: 0, mtime: 0 }),
            NodeEntry::File(f) => Some(Stat { kind: NodeKind::File, size: f.size, mtime: f.mtime }),
            NodeEntry::Tombstone => None,
        }
    }
```

(i) `readdir` — the child-mapping match gains a Tombstone arm:

```rust
                match &node.entry {
                    NodeEntry::Dir(_) => DirEntry { name: node.name.clone(), kind: NodeKind::Dir, size: 0, mtime: 0 },
                    NodeEntry::File(f) => DirEntry { name: node.name.clone(), kind: NodeKind::File, size: f.size, mtime: f.mtime },
                    NodeEntry::Tombstone => DirEntry { name: node.name.clone(), kind: NodeKind::Tombstone, size: 0, mtime: 0 },
                }
```

(j) `WalkNodeKind` gains a `Tombstone` variant, and `walk_from` emits it:

```rust
pub enum WalkNodeKind<'a> {
    Dir,
    File { source: &'a [u8], size: u64, mtime: i64, layer: LayerId, cache_key: crate::model::CacheKey },
    Tombstone,
}
```

In `walk_from`, add an arm after the `File` arm:

```rust
            NodeEntry::Tombstone => {
                visit(WalkNode {
                    id: idx,
                    display: &node.name,
                    folded: folded_name.to_string(),
                    kind: WalkNodeKind::Tombstone,
                    children: Vec::new(),
                });
            }
```

- [ ] **Step 5: Run to verify passing**

Run: `cargo test -p vfs-core`
Expected: PASS — including the 3 updated tests (Step 2a) and the 2 new tests
(Step 2b). If a pre-existing test does a non-exhaustive match on
`NodeKind`/`Resolution`/`WalkNodeKind`, add the `Tombstone` arm there too
(compile-guided).

- [ ] **Step 6: Commit**

```bash
git add crates/vfs-core/src/model.rs crates/vfs-core/src/tree.rs
git commit -m "vfs-core: first-class tombstone nodes (resolve/getattr/readdir/walk)"
```

---

### Task 2: `vfs-shared` — carry tombstones through the snapshot

**Files:**
- Modify: `crates/vfs-shared/src/layout.rs` (`KIND_TOMBSTONE`)
- Modify: `crates/vfs-shared/src/builder.rs` (`add_tombstone`)
- Modify: `crates/vfs-shared/src/reader.rs` (`NodeKind`/`SnapResolution` + recognition)
- Modify: `crates/vfs-shared/src/bridge.rs` (map `WalkNodeKind::Tombstone`)
- Test: `crates/vfs-shared/src/builder.rs` and `crates/vfs-shared/src/reader.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `vfs_core::WalkNodeKind::Tombstone` (Task 1).
- Produces: `KIND_TOMBSTONE`, `SnapshotBuilder::add_tombstone(display) -> u32`, `NodeKind::Tombstone`, `SnapResolution::Tombstone`; reader `getattr` → `None`, `readdir` includes tombstones; `bridge::flatten` emits tombstone nodes.

- [ ] **Step 1: Add `KIND_TOMBSTONE`**

In `crates/vfs-shared/src/layout.rs`, after `pub const KIND_FILE: u8 = 1;`:

```rust
pub const KIND_TOMBSTONE: u8 = 2;
```

- [ ] **Step 2: Write the failing tests**

In `crates/vfs-shared/src/builder.rs` tests, add:

```rust
    #[test]
    fn tombstone_node_kind_is_written() {
        let mut bld = SnapshotBuilder::new();
        let t = bld.add_tombstone("gone.esp");
        bld.set_root(t);
        let img = bld.finish();
        assert_eq!(read_u8(&img, HEADER_SIZE + N_KIND), Some(KIND_TOMBSTONE));
    }
```

In `crates/vfs-shared/src/reader.rs` tests, add (uses the existing `fixture`
pattern; build a small tree with one tombstone child under `data`):

```rust
    #[test]
    fn reader_surfaces_tombstones() {
        let mut b = SnapshotBuilder::new();
        let gone = b.add_tombstone("gone.esp");
        let data = b.add_dir("data", &[("gone.esp".into(), gone)]);
        let root = b.add_dir("", &[("data".into(), data)]);
        b.set_root(root);
        let img = b.finish();
        let r = SnapshotReader::open(&img).unwrap();

        assert_eq!(r.resolve(&["data", "gone.esp"]), SnapResolution::Tombstone);
        assert_eq!(r.getattr(&["data", "gone.esp"]), None);
        let entries = r.readdir(&["data"]).unwrap();
        let e = entries.iter().find(|e| e.name == "gone.esp").unwrap();
        assert_eq!(e.kind, NodeKind::Tombstone);
    }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p vfs-shared tombstone`
Expected: FAIL to compile (`add_tombstone`, `KIND_TOMBSTONE` in test scope,
`SnapResolution::Tombstone`, `NodeKind::Tombstone` missing).

- [ ] **Step 4: Implement the builder + reader + bridge**

(a) `builder.rs` — add `add_tombstone` (mirrors `add_file`, no source/size):

```rust
    pub fn add_tombstone(&mut self, display: &str) -> u32 {
        let (name_off, name_len) = self.intern(display.as_bytes());
        let id = self.nodes.len() as u32;
        self.nodes.push(NodeRec {
            kind: KIND_TOMBSTONE,
            layer: 0,
            name_off,
            name_len,
            child_first: 0,
            child_count: 0,
            source_off: 0,
            source_len: 0,
            size: 0,
            mtime: 0,
            cache_key: [0; 32],
        });
        id
    }
```

(b) `reader.rs` — `NodeKind` gains `Tombstone`; `SnapResolution` gains `Tombstone`:

```rust
pub enum NodeKind { Dir, File, Tombstone }
```
```rust
pub enum SnapResolution {
    File { source: Vec<u8>, size: u64, mtime: i64, layer: u32, cache_key: [u8; 32] },
    Dir,
    Tombstone,
    NotFound,
}
```

(c) `reader.rs` `node_kind` — recognize the new byte:

```rust
        match read_u8(self.bytes, base + N_KIND)? {
            KIND_DIR => Some(NodeKind::Dir),
            KIND_FILE => Some(NodeKind::File),
            KIND_TOMBSTONE => Some(NodeKind::Tombstone),
            _ => None,
        }
```

(d) `reader.rs` `getattr` — tombstone has no stat:

```rust
        match self.node_kind(idx)? {
            NodeKind::Dir => Some(SnapStat { kind: NodeKind::Dir, size: 0, mtime: 0 }),
            NodeKind::File => Some(SnapStat {
                kind: NodeKind::File,
                size: read_u64(self.bytes, base + N_SIZE)?,
                mtime: read_i64(self.bytes, base + N_MTIME)?,
            }),
            NodeKind::Tombstone => None,
        }
```

(e) `reader.rs` `resolve` — add the arm:

```rust
        match self.node_kind(idx) {
            Some(NodeKind::Dir) => SnapResolution::Dir,
            Some(NodeKind::File) => self.file_resolution(base).unwrap_or(SnapResolution::NotFound),
            Some(NodeKind::Tombstone) => SnapResolution::Tombstone,
            None => SnapResolution::NotFound,
        }
```

(f) `reader.rs` `readdir` — the per-child size/kind match gains a Tombstone arm
(size 0), keeping the entry in the list:

```rust
                    let (size, mtime) = match kind {
                        NodeKind::Dir => (0, 0),
                        NodeKind::Tombstone => (0, 0),
                        NodeKind::File => (
                            read_u64(self.bytes, nb + N_SIZE).unwrap_or(0),
                            read_i64(self.bytes, nb + N_MTIME).unwrap_or(0),
                        ),
                    };
```

(g) `bridge.rs` — map the walk variant. In the `match &n.kind` block add:

```rust
            WalkNodeKind::Tombstone => builder.add_tombstone(n.display),
```

- [ ] **Step 5: Run to verify passing**

Run: `cargo test -p vfs-shared`
Expected: PASS (existing + 2 new tombstone tests). Fix any compile-guided
non-exhaustive matches by adding the `Tombstone` arm.

- [ ] **Step 6: End-to-end parity test (bridge)**

Add to `crates/vfs-shared/src/bridge.rs` tests a case that builds a `vfs-core`
tree containing a tombstone, `flatten`s it, and checks the reader agrees:

```rust
    #[test]
    fn flatten_preserves_tombstones() {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let entry = |vpath: &str, kind: EntryKind| InputEntry {
            vpath: vpath.into(), kind, source: "s".into(), size: 0, mtime: 0,
        };
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![entry("data/keep.esp", EntryKind::File), entry("data/gone.esp", EntryKind::Tombstone)],
        }])
        .unwrap();
        let img = flatten(&tree);
        let r = crate::reader::SnapshotReader::open(&img).unwrap();
        assert_eq!(r.resolve(&["data", "gone.esp"]), crate::reader::SnapResolution::Tombstone);
        assert!(matches!(r.resolve(&["data", "keep.esp"]), crate::reader::SnapResolution::File { .. }));
    }
```

Run: `cargo test -p vfs-shared flatten_preserves_tombstones`
Expected: PASS. (Adjust the `entry` helper's `source`/field names only if the
existing bridge test helper differs — mirror what's already there.)

- [ ] **Step 7: Commit**

```bash
git add crates/vfs-shared/src/layout.rs crates/vfs-shared/src/builder.rs crates/vfs-shared/src/reader.rs crates/vfs-shared/src/bridge.rs
git commit -m "vfs-shared: carry tombstones through the snapshot (KIND_TOMBSTONE + reader)"
```

---

### Task 3: Verification sweep

**Files:** none.

- [ ] **Step 1: Full workspace test**

Run: `cargo test --workspace`
Expected: all green. In particular, `vfs-shim`/`vfs-redirect` still compile — they
match on `SnapResolution` in `decide`; adding a variant may force a `Tombstone`
arm there. If so, the minimal correct behavior for THIS slice is to treat
`SnapResolution::Tombstone` the same as it currently treats a non-file (i.e.
`Decision::PassThrough` in `vfs-redirect`'s `decide`) — a TODO the next slice
(B) replaces with a real Deny. Add that arm if the compiler requires it, commit
it with the message `vfs-redirect: pass-through arm for SnapResolution::Tombstone (interim)`, and note it.

- [ ] **Step 2: Warning check**

Run: `cargo build --workspace 2>&1 | rg -i "warning" || echo "no warnings"`
Expected: `no warnings`.

- [ ] **Step 3: Commit Cargo.lock (if changed)**

```bash
git add Cargo.lock
git commit -m "tombstones: update Cargo.lock" || echo "nothing to commit"
```

---

## Self-Review Notes

- **Spec coverage:** node variant + build insert + resolve/getattr/readdir/walk (Task 1); `KIND_TOMBSTONE` + builder + reader recognition + bridge + parity (Task 2); downstream compile check (Task 3). Every §6 test case is present.
- **Derives:** all touched enums keep their existing `Debug`/`PartialEq`/`Eq`(/`Copy`) derives, so `assert_eq!`/`matches!` compile; `getattr` returns `Option` so `== None` works.
- **Interim downstream arm:** `vfs-redirect::decide` matches `SnapResolution`; Task 3 Step 1 handles the forced `Tombstone` arm as interim `PassThrough` (Slice B makes it a real Deny). This is the one place a `Tombstone` is knowingly not-yet-honored, and it's called out.
- **No format break:** `KIND_TOMBSTONE` is a new value in the existing `kind` byte; `NODE_SIZE`/offsets and the layout size asserts are unchanged.
