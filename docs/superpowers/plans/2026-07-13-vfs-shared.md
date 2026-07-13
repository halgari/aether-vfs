# vfs-shared Snapshot Layout Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `vfs-shared` — a bitness-neutral `#[repr(C)]` shared-memory tree-snapshot layout with a post-order builder, a fold-free bounds-checked reader, and a seqlock publish/read protocol.

**Architecture:** A new workspace crate `crates/vfs-shared` operating purely on `&[u8]`/`&mut [u8]`. Snapshot format is `[Header][Node array][Child array][String blob]`, all fixed-width little-endian, offsets single-sourced via `offset_of!`. The reader bounds-checks every access (torn-read safe); the seqlock uses one audited atomic view of the generation field. A feature-gated `bridge` flattens a `vfs-core::VfsTree` into a snapshot for a round-trip test.

**Tech Stack:** Rust (stable). No dependencies in the default build. Feature `bridge` → `vfs-core` (path dep).

## Global Constraints

- **Toolchain:** stable Rust.
- **Unsafe:** crate root is `#![deny(unsafe_code)]`. Exactly ONE `#[allow(unsafe_code)]` is permitted — the seqlock generation accessor in `seqlock.rs` — with a `// SAFETY:` comment. No other `unsafe` anywhere.
- **No dependencies** in the default build. `vfs-core` appears only under the `bridge` feature and as a dev-dependency.
- **Bitness-neutral:** fixed-width fields only (`u8`/`u32`/`u64`/`i64`/`[u8;N]`), `u32` indices, absolute `u32` byte offsets. No `usize`/pointers in the layout. Little-endian, same-machine.
- **No panics** on any input or on torn data: every buffer access is bounds-checked and returns `None`/`Err`, never indexes out of range.
- **Format constants:** `MAGIC = 0x5646_5353`, `VERSION = 1`, `KIND_DIR = 0`, `KIND_FILE = 1`.
- **Struct sizes (asserted):** `Header` = 48, `SnapNode` = 80, `SnapChild` = 16 bytes.
- **Fold contract:** the builder and any reader-caller must fold names with the same function; `vfs-shared` itself never folds (raw byte compares). The `bridge` folds via `vfs-core`.

## Parallelization note

Task 8 (the `vfs-core` walk API) lives in a **different crate** and is independent of vfs-shared Tasks 1–7; it only needs to land before Task 9 (bridge). It may be executed in **parallel** with Tasks 1–7 **if run in a separate git worktree** (a concurrent commit to the shared index otherwise races). Tasks 1–7 are sequential (they build up `crates/vfs-shared` and share `lib.rs`). Tasks 9–10 depend on 8 + the vfs-shared core.

```
1 → 2 → 3 → 4 → 5 → 6 → 7         (vfs-shared, sequential)
              8 (vfs-core walk)    (independent crate; parallel-capable via worktree)
                    ↓
              9 (bridge) → 10 (vfs-core round-trip)
```

---

### Task 1: Scaffold `vfs-shared` crate

**Files:**
- Modify: `Cargo.toml` (workspace root — add member)
- Create: `crates/vfs-shared/Cargo.toml`
- Create: `crates/vfs-shared/src/lib.rs`
- Create: `crates/vfs-shared/src/layout.rs`, `builder.rs`, `reader.rs`, `seqlock.rs` (placeholder doc-comment lines)

**Interfaces:**
- Consumes: nothing.
- Produces: a compiling `vfs-shared` library with modules declared and `#![deny(unsafe_code)]`.

- [ ] **Step 1: Add the crate to the workspace members**

Edit `Cargo.toml` (root) so `members` reads:

```toml
[workspace]
resolver = "2"
members = ["crates/vfs-core", "crates/vfs-shared"]
```

- [ ] **Step 2: Create `crates/vfs-shared/Cargo.toml`**

```toml
[package]
name = "vfs-shared"
version = "0.1.0"
edition = "2021"

[features]
bridge = ["dep:vfs-core"]

[dependencies]
vfs-core = { path = "../vfs-core", optional = true }

[dev-dependencies]
vfs-core = { path = "../vfs-core" }
```

- [ ] **Step 3: Create `crates/vfs-shared/src/lib.rs`**

```rust
#![deny(unsafe_code)]
//! `vfs-shared`: bitness-neutral shared-memory snapshot layout for the virtual
//! tree. Pure byte-buffer operations; the OS shared-memory mapping lives
//! elsewhere. Layout/builder/reader are unsafe-free; the seqlock has one audited
//! atomic view.

pub mod layout;
pub mod builder;
pub mod reader;
pub mod seqlock;

#[cfg(feature = "bridge")]
pub mod bridge;

// pub use lines are added by later tasks as items land.
```

- [ ] **Step 4: Create placeholder module files**

- `crates/vfs-shared/src/layout.rs` → `//! Snapshot byte layout: structs, offsets, LE field helpers.`
- `crates/vfs-shared/src/builder.rs` → `//! SnapshotBuilder.`
- `crates/vfs-shared/src/reader.rs` → `//! SnapshotReader.`
- `crates/vfs-shared/src/seqlock.rs` → `//! Seqlock publish / read_stable.`

- [ ] **Step 5: Verify it builds**

Run: `cargo build -p vfs-shared`
Expected: compiles clean (empty modules).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/vfs-shared
git commit -m "chore: scaffold vfs-shared crate"
```

---

### Task 2: Layout — structs, offsets, LE helpers (`layout.rs`)

**Files:**
- Modify: `crates/vfs-shared/src/layout.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - Constants `MAGIC`, `VERSION`, `KIND_DIR`, `KIND_FILE`, `HEADER_SIZE`, `NODE_SIZE`, `CHILD_SIZE`, and all field-offset consts (`H_*`, `N_*`, `C_*`).
  - `#[repr(C)]` `Header`, `SnapNode`, `SnapChild` with compile-time size/align asserts.
  - Bounds-checked LE readers `read_u8/read_u32/read_u64/read_i64/read_key/read_slice` and writers `write_u32/write_u64/write_i64/write_u8/write_key/write_bytes`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/vfs-shared/src/layout.rs`:

```rust
use core::mem::{align_of, offset_of, size_of};

pub const MAGIC: u32 = 0x5646_5353;
pub const VERSION: u32 = 1;
pub const KIND_DIR: u8 = 0;
pub const KIND_FILE: u8 = 1;

#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub version: u32,
    pub generation: u64,
    pub total_len: u32,
    pub root_node: u32,
    pub node_count: u32,
    pub nodes_off: u32,
    pub child_count: u32,
    pub children_off: u32,
    pub strings_len: u32,
    pub strings_off: u32,
}

#[repr(C)]
pub struct SnapNode {
    pub kind: u8,
    pub _pad0: [u8; 3],
    pub layer: u32,
    pub name_off: u32,
    pub name_len: u32,
    pub child_first: u32,
    pub child_count: u32,
    pub source_off: u32,
    pub source_len: u32,
    pub size: u64,
    pub mtime: i64,
    pub cache_key: [u8; 32],
}

#[repr(C)]
pub struct SnapChild {
    pub folded_off: u32,
    pub folded_len: u32,
    pub node: u32,
    pub _pad: u32,
}

pub const HEADER_SIZE: usize = size_of::<Header>();
pub const NODE_SIZE: usize = size_of::<SnapNode>();
pub const CHILD_SIZE: usize = size_of::<SnapChild>();

const _: () = assert!(HEADER_SIZE == 48 && align_of::<Header>() == 8);
const _: () = assert!(NODE_SIZE == 80 && align_of::<SnapNode>() == 8);
const _: () = assert!(CHILD_SIZE == 16);

pub const H_MAGIC: usize = offset_of!(Header, magic);
pub const H_VERSION: usize = offset_of!(Header, version);
pub const H_GENERATION: usize = offset_of!(Header, generation);
pub const H_TOTAL_LEN: usize = offset_of!(Header, total_len);
pub const H_ROOT_NODE: usize = offset_of!(Header, root_node);
pub const H_NODE_COUNT: usize = offset_of!(Header, node_count);
pub const H_NODES_OFF: usize = offset_of!(Header, nodes_off);
pub const H_CHILD_COUNT: usize = offset_of!(Header, child_count);
pub const H_CHILDREN_OFF: usize = offset_of!(Header, children_off);
pub const H_STRINGS_LEN: usize = offset_of!(Header, strings_len);
pub const H_STRINGS_OFF: usize = offset_of!(Header, strings_off);

pub const N_KIND: usize = offset_of!(SnapNode, kind);
pub const N_LAYER: usize = offset_of!(SnapNode, layer);
pub const N_NAME_OFF: usize = offset_of!(SnapNode, name_off);
pub const N_NAME_LEN: usize = offset_of!(SnapNode, name_len);
pub const N_CHILD_FIRST: usize = offset_of!(SnapNode, child_first);
pub const N_CHILD_COUNT: usize = offset_of!(SnapNode, child_count);
pub const N_SOURCE_OFF: usize = offset_of!(SnapNode, source_off);
pub const N_SOURCE_LEN: usize = offset_of!(SnapNode, source_len);
pub const N_SIZE: usize = offset_of!(SnapNode, size);
pub const N_MTIME: usize = offset_of!(SnapNode, mtime);
pub const N_CACHE_KEY: usize = offset_of!(SnapNode, cache_key);

pub const C_FOLDED_OFF: usize = offset_of!(SnapChild, folded_off);
pub const C_FOLDED_LEN: usize = offset_of!(SnapChild, folded_len);
pub const C_NODE: usize = offset_of!(SnapChild, node);

pub fn read_u8(b: &[u8], off: usize) -> Option<u8> {
    b.get(off).copied()
}
pub fn read_u32(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes(s.try_into().ok()?))
}
pub fn read_u64(b: &[u8], off: usize) -> Option<u64> {
    let s = b.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes(s.try_into().ok()?))
}
pub fn read_i64(b: &[u8], off: usize) -> Option<i64> {
    let s = b.get(off..off.checked_add(8)?)?;
    Some(i64::from_le_bytes(s.try_into().ok()?))
}
pub fn read_key(b: &[u8], off: usize) -> Option<[u8; 32]> {
    let s = b.get(off..off.checked_add(32)?)?;
    Some(s.try_into().ok()?)
}
pub fn read_slice(b: &[u8], off: usize, len: usize) -> Option<&[u8]> {
    b.get(off..off.checked_add(len)?)
}

pub fn write_u8(b: &mut [u8], off: usize, v: u8) {
    b[off] = v;
}
pub fn write_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
pub fn write_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
pub fn write_i64(b: &mut [u8], off: usize, v: i64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
pub fn write_key(b: &mut [u8], off: usize, v: &[u8; 32]) {
    b[off..off + 32].copy_from_slice(v);
}
pub fn write_bytes(b: &mut [u8], off: usize, v: &[u8]) {
    b[off..off + v.len()].copy_from_slice(v);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_offsets_are_expected() {
        assert_eq!(H_MAGIC, 0);
        assert_eq!(H_VERSION, 4);
        assert_eq!(H_GENERATION, 8);
        assert_eq!(H_TOTAL_LEN, 16);
        assert_eq!(H_STRINGS_OFF, 44);
        assert_eq!(HEADER_SIZE, 48);
    }

    #[test]
    fn node_offsets_are_expected() {
        assert_eq!(N_KIND, 0);
        assert_eq!(N_LAYER, 4);
        assert_eq!(N_SIZE, 32);
        assert_eq!(N_MTIME, 40);
        assert_eq!(N_CACHE_KEY, 48);
        assert_eq!(NODE_SIZE, 80);
    }

    #[test]
    fn le_roundtrip() {
        let mut b = vec![0u8; 64];
        write_u32(&mut b, 4, 0xDEAD_BEEF);
        write_u64(&mut b, 8, 0x0102_0304_0506_0708);
        write_i64(&mut b, 16, -42);
        assert_eq!(read_u32(&b, 4), Some(0xDEAD_BEEF));
        assert_eq!(read_u64(&b, 8), Some(0x0102_0304_0506_0708));
        assert_eq!(read_i64(&b, 16), Some(-42));
    }

    #[test]
    fn reads_out_of_bounds_return_none() {
        let b = vec![0u8; 4];
        assert_eq!(read_u32(&b, 2), None);
        assert_eq!(read_u64(&b, 0), None);
        assert_eq!(read_slice(&b, 3, 4), None);
        assert_eq!(read_u8(&b, 4), None);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail/compile**

Run: `cargo test -p vfs-shared layout`
Expected: The compile-time `assert!`s must hold (if a size is wrong the crate won't compile — that is the layout guard doing its job). The four runtime tests PASS once it compiles. If the crate fails to compile on a `const _: ()` assert, the struct layout is wrong — STOP and report.

- [ ] **Step 3: (No separate impl step — the code above is complete.) Confirm the module is wired**

The `layout` module is already declared `pub mod layout;` in lib.rs (Task 1). No lib.rs change needed.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vfs-shared layout`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-shared/src/layout.rs
git commit -m "feat(vfs-shared): snapshot byte layout, offsets, LE helpers"
```

---

### Task 3: Builder (`builder.rs`)

**Files:**
- Modify: `crates/vfs-shared/src/builder.rs`
- Modify: `crates/vfs-shared/src/lib.rs` (add `pub use builder::SnapshotBuilder;`)

**Interfaces:**
- Consumes: `layout::*`.
- Produces: `pub struct SnapshotBuilder` with `new`, `add_file`, `add_dir`, `set_root`, `finish`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/vfs-shared/src/builder.rs`:

```rust
use std::collections::HashMap;

use crate::layout::*;

struct NodeRec {
    kind: u8,
    layer: u32,
    name_off: u32,
    name_len: u32,
    child_first: u32,
    child_count: u32,
    source_off: u32,
    source_len: u32,
    size: u64,
    mtime: i64,
    cache_key: [u8; 32],
}

struct ChildRec {
    folded_off: u32,
    folded_len: u32,
    node: u32,
}

/// Builds a snapshot image bottom-up (children before their parent dir).
pub struct SnapshotBuilder {
    strings: Vec<u8>,
    intern: HashMap<Vec<u8>, u32>,
    nodes: Vec<NodeRec>,
    children: Vec<ChildRec>,
    root: u32,
}

impl SnapshotBuilder {
    pub fn new() -> Self {
        SnapshotBuilder {
            strings: Vec::new(),
            intern: HashMap::new(),
            nodes: Vec::new(),
            children: Vec::new(),
            root: 0,
        }
    }

    fn intern(&mut self, bytes: &[u8]) -> (u32, u32) {
        if let Some(&off) = self.intern.get(bytes) {
            return (off, bytes.len() as u32);
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(bytes);
        self.intern.insert(bytes.to_vec(), off);
        (off, bytes.len() as u32)
    }

    pub fn add_file(
        &mut self,
        display: &str,
        source: &[u8],
        size: u64,
        mtime: i64,
        layer: u32,
        cache_key: [u8; 32],
    ) -> u32 {
        let (name_off, name_len) = self.intern(display.as_bytes());
        let (source_off, source_len) = self.intern(source);
        let id = self.nodes.len() as u32;
        self.nodes.push(NodeRec {
            kind: KIND_FILE,
            layer,
            name_off,
            name_len,
            child_first: 0,
            child_count: 0,
            source_off,
            source_len,
            size,
            mtime,
            cache_key,
        });
        id
    }

    pub fn add_dir(&mut self, display: &str, children: &[(String, u32)]) -> u32 {
        let mut sorted = children.to_vec();
        sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let child_first = self.children.len() as u32;
        for (folded, node) in &sorted {
            let (folded_off, folded_len) = self.intern(folded.as_bytes());
            self.children.push(ChildRec { folded_off, folded_len, node: *node });
        }
        let (name_off, name_len) = self.intern(display.as_bytes());
        let id = self.nodes.len() as u32;
        self.nodes.push(NodeRec {
            kind: KIND_DIR,
            layer: 0,
            name_off,
            name_len,
            child_first,
            child_count: sorted.len() as u32,
            source_off: 0,
            source_len: 0,
            size: 0,
            mtime: 0,
            cache_key: [0; 32],
        });
        id
    }

    pub fn set_root(&mut self, node: u32) {
        self.root = node;
    }

    pub fn finish(self) -> Vec<u8> {
        let node_count = self.nodes.len();
        let child_count = self.children.len();
        let nodes_off = HEADER_SIZE;
        let children_off = nodes_off + node_count * NODE_SIZE;
        let strings_off = children_off + child_count * CHILD_SIZE;
        let total_len = strings_off + self.strings.len();

        let mut b = vec![0u8; total_len];
        write_u32(&mut b, H_MAGIC, MAGIC);
        write_u32(&mut b, H_VERSION, VERSION);
        write_u64(&mut b, H_GENERATION, 0);
        write_u32(&mut b, H_TOTAL_LEN, total_len as u32);
        write_u32(&mut b, H_ROOT_NODE, self.root);
        write_u32(&mut b, H_NODE_COUNT, node_count as u32);
        write_u32(&mut b, H_NODES_OFF, nodes_off as u32);
        write_u32(&mut b, H_CHILD_COUNT, child_count as u32);
        write_u32(&mut b, H_CHILDREN_OFF, children_off as u32);
        write_u32(&mut b, H_STRINGS_LEN, self.strings.len() as u32);
        write_u32(&mut b, H_STRINGS_OFF, strings_off as u32);

        let s = strings_off as u32;
        for (i, n) in self.nodes.iter().enumerate() {
            let base = nodes_off + i * NODE_SIZE;
            write_u8(&mut b, base + N_KIND, n.kind);
            write_u32(&mut b, base + N_LAYER, n.layer);
            write_u32(&mut b, base + N_NAME_OFF, s + n.name_off);
            write_u32(&mut b, base + N_NAME_LEN, n.name_len);
            write_u32(&mut b, base + N_CHILD_FIRST, n.child_first);
            write_u32(&mut b, base + N_CHILD_COUNT, n.child_count);
            write_u32(&mut b, base + N_SOURCE_OFF, s + n.source_off);
            write_u32(&mut b, base + N_SOURCE_LEN, n.source_len);
            write_u64(&mut b, base + N_SIZE, n.size);
            write_i64(&mut b, base + N_MTIME, n.mtime);
            write_key(&mut b, base + N_CACHE_KEY, &n.cache_key);
        }
        for (j, c) in self.children.iter().enumerate() {
            let base = children_off + j * CHILD_SIZE;
            write_u32(&mut b, base + C_FOLDED_OFF, s + c.folded_off);
            write_u32(&mut b, base + C_FOLDED_LEN, c.folded_len);
            write_u32(&mut b, base + C_NODE, c.node);
        }
        write_bytes(&mut b, strings_off, &self.strings);
        b
    }
}

impl Default for SnapshotBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_reflects_counts_and_offsets() {
        let mut bld = SnapshotBuilder::new();
        let f1 = bld.add_file("a.esp", b"src/a", 10, 1, 0, [0; 32]);
        let f2 = bld.add_file("b.esp", b"src/b", 20, 2, 0, [0; 32]);
        let root = bld.add_dir("", &[("a.esp".into(), f1), ("b.esp".into(), f2)]);
        bld.set_root(root);
        let img = bld.finish();

        assert_eq!(read_u32(&img, H_MAGIC), Some(MAGIC));
        assert_eq!(read_u32(&img, H_VERSION), Some(VERSION));
        assert_eq!(read_u32(&img, H_NODE_COUNT), Some(3));
        assert_eq!(read_u32(&img, H_CHILD_COUNT), Some(2));
        assert_eq!(read_u32(&img, H_ROOT_NODE), Some(root));
        assert_eq!(read_u32(&img, H_TOTAL_LEN), Some(img.len() as u32));
        // nodes start right after the header
        assert_eq!(read_u32(&img, H_NODES_OFF), Some(HEADER_SIZE as u32));
    }

    #[test]
    fn file_node_fields_are_written() {
        let mut bld = SnapshotBuilder::new();
        let f = bld.add_file("a.esp", b"src/a", 10, 7, 3, [9; 32]);
        bld.set_root(f);
        let img = bld.finish();
        let base = HEADER_SIZE; // node 0
        assert_eq!(read_u8(&img, base + N_KIND), Some(KIND_FILE));
        assert_eq!(read_u64(&img, base + N_SIZE), Some(10));
        assert_eq!(read_i64(&img, base + N_MTIME), Some(7));
        assert_eq!(read_u32(&img, base + N_LAYER), Some(3));
        assert_eq!(read_key(&img, base + N_CACHE_KEY), Some([9; 32]));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vfs-shared builder`
Expected: FAIL to compile first if `pub use` missing — but tests are in-module so they compile. They should PASS once the code compiles (the implementation is included above). Run and confirm PASS; if a test fails, STOP and report.

- [ ] **Step 3: Add the re-export in `lib.rs`**

Add: `pub use builder::SnapshotBuilder;`
Run: `cargo build -p vfs-shared`
Expected: compiles.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vfs-shared builder`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-shared/src/builder.rs crates/vfs-shared/src/lib.rs
git commit -m "feat(vfs-shared): post-order snapshot builder"
```

---

### Task 4: Reader (`reader.rs`)

**Files:**
- Modify: `crates/vfs-shared/src/reader.rs`
- Modify: `crates/vfs-shared/src/lib.rs` (add reader re-exports)

**Interfaces:**
- Consumes: `layout::*`.
- Produces: `SnapshotReader`, `NodeKind`, `SnapStat`, `SnapDirEntry`, `SnapResolution`, `LayoutError`, `ReadError`, with `open`/`generation`/`root`/`getattr`/`resolve`/`readdir`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/vfs-shared/src/reader.rs`:

```rust
use crate::layout::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    TooSmall,
    BadMagic,
    BadVersion,
    RegionOutOfBounds,
    BadRoot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    NotADirectory,
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapStat {
    pub kind: NodeKind,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapDirEntry {
    pub name: String,
    pub kind: NodeKind,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapResolution {
    File {
        source: Vec<u8>,
        size: u64,
        mtime: i64,
        layer: u32,
        cache_key: [u8; 32],
    },
    Dir,
    NotFound,
}

pub struct SnapshotReader<'a> {
    bytes: &'a [u8],
    nodes_off: usize,
    node_count: u32,
    children_off: usize,
    child_count: u32,
    root_node: u32,
}

impl<'a> SnapshotReader<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self, LayoutError> {
        if bytes.len() < HEADER_SIZE {
            return Err(LayoutError::TooSmall);
        }
        if read_u32(bytes, H_MAGIC) != Some(MAGIC) {
            return Err(LayoutError::BadMagic);
        }
        if read_u32(bytes, H_VERSION) != Some(VERSION) {
            return Err(LayoutError::BadVersion);
        }
        let total_len = read_u32(bytes, H_TOTAL_LEN).unwrap() as usize;
        if total_len > bytes.len() {
            return Err(LayoutError::RegionOutOfBounds);
        }
        let node_count = read_u32(bytes, H_NODE_COUNT).unwrap();
        let nodes_off = read_u32(bytes, H_NODES_OFF).unwrap() as usize;
        let child_count = read_u32(bytes, H_CHILD_COUNT).unwrap();
        let children_off = read_u32(bytes, H_CHILDREN_OFF).unwrap() as usize;
        let strings_off = read_u32(bytes, H_STRINGS_OFF).unwrap() as usize;
        let strings_len = read_u32(bytes, H_STRINGS_LEN).unwrap() as usize;
        let root_node = read_u32(bytes, H_ROOT_NODE).unwrap();

        // Every region must fit within total_len.
        let nodes_end = nodes_off.checked_add((node_count as usize).checked_mul(NODE_SIZE).ok_or(LayoutError::RegionOutOfBounds)?).ok_or(LayoutError::RegionOutOfBounds)?;
        let children_end = children_off.checked_add((child_count as usize).checked_mul(CHILD_SIZE).ok_or(LayoutError::RegionOutOfBounds)?).ok_or(LayoutError::RegionOutOfBounds)?;
        let strings_end = strings_off.checked_add(strings_len).ok_or(LayoutError::RegionOutOfBounds)?;
        if nodes_end > total_len || children_end > total_len || strings_end > total_len {
            return Err(LayoutError::RegionOutOfBounds);
        }
        if node_count > 0 && root_node >= node_count {
            return Err(LayoutError::BadRoot);
        }
        Ok(SnapshotReader {
            bytes,
            nodes_off,
            node_count,
            children_off,
            child_count,
            root_node,
        })
    }

    pub fn generation(&self) -> u64 {
        read_u64(self.bytes, H_GENERATION).unwrap_or(0)
    }

    pub fn root(&self) -> u32 {
        self.root_node
    }

    fn node_base(&self, idx: u32) -> Option<usize> {
        if idx >= self.node_count {
            return None;
        }
        Some(self.nodes_off + idx as usize * NODE_SIZE)
    }

    fn node_kind(&self, idx: u32) -> Option<NodeKind> {
        let base = self.node_base(idx)?;
        match read_u8(self.bytes, base + N_KIND)? {
            KIND_DIR => Some(NodeKind::Dir),
            KIND_FILE => Some(NodeKind::File),
            _ => None,
        }
    }

    fn node_name(&self, idx: u32) -> Option<String> {
        let base = self.node_base(idx)?;
        let off = read_u32(self.bytes, base + N_NAME_OFF)? as usize;
        let len = read_u32(self.bytes, base + N_NAME_LEN)? as usize;
        let s = read_slice(self.bytes, off, len)?;
        Some(String::from_utf8_lossy(s).into_owned())
    }

    /// Resolve a folded path to a node index, bounds-checked throughout.
    fn lookup(&self, folded: &[&str]) -> Option<u32> {
        if self.node_count == 0 {
            return None;
        }
        let mut cur = self.root_node;
        for comp in folded {
            if self.node_kind(cur)? != NodeKind::Dir {
                return None;
            }
            cur = self.find_child(cur, comp.as_bytes())?;
        }
        Some(cur)
    }

    /// Binary-search a dir's child run for a folded name.
    fn find_child(&self, dir: u32, folded: &[u8]) -> Option<u32> {
        let base = self.node_base(dir)?;
        let first = read_u32(self.bytes, base + N_CHILD_FIRST)?;
        let count = read_u32(self.bytes, base + N_CHILD_COUNT)?;
        let (mut lo, mut hi) = (0u32, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let cidx = first.checked_add(mid)?;
            if cidx >= self.child_count {
                return None;
            }
            let cbase = self.children_off + cidx as usize * CHILD_SIZE;
            let foff = read_u32(self.bytes, cbase + C_FOLDED_OFF)? as usize;
            let flen = read_u32(self.bytes, cbase + C_FOLDED_LEN)? as usize;
            let name = read_slice(self.bytes, foff, flen)?;
            match name.cmp(folded) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    return read_u32(self.bytes, cbase + C_NODE);
                }
            }
        }
        None
    }

    pub fn getattr(&self, folded: &[&str]) -> Option<SnapStat> {
        let idx = self.lookup(folded)?;
        let base = self.node_base(idx)?;
        match self.node_kind(idx)? {
            NodeKind::Dir => Some(SnapStat { kind: NodeKind::Dir, size: 0, mtime: 0 }),
            NodeKind::File => Some(SnapStat {
                kind: NodeKind::File,
                size: read_u64(self.bytes, base + N_SIZE)?,
                mtime: read_i64(self.bytes, base + N_MTIME)?,
            }),
        }
    }

    fn file_resolution(&self, base: usize) -> Option<SnapResolution> {
        let off = read_u32(self.bytes, base + N_SOURCE_OFF)? as usize;
        let len = read_u32(self.bytes, base + N_SOURCE_LEN)? as usize;
        let source = read_slice(self.bytes, off, len)?.to_vec();
        Some(SnapResolution::File {
            source,
            size: read_u64(self.bytes, base + N_SIZE)?,
            mtime: read_i64(self.bytes, base + N_MTIME)?,
            layer: read_u32(self.bytes, base + N_LAYER)?,
            cache_key: read_key(self.bytes, base + N_CACHE_KEY)?,
        })
    }

    pub fn resolve(&self, folded: &[&str]) -> SnapResolution {
        let idx = match self.lookup(folded) {
            Some(i) => i,
            None => return SnapResolution::NotFound,
        };
        let base = match self.node_base(idx) {
            Some(b) => b,
            None => return SnapResolution::NotFound,
        };
        match self.node_kind(idx) {
            Some(NodeKind::Dir) => SnapResolution::Dir,
            Some(NodeKind::File) => self.file_resolution(base).unwrap_or(SnapResolution::NotFound),
            None => SnapResolution::NotFound,
        }
    }

    pub fn readdir(&self, folded: &[&str]) -> Result<Vec<SnapDirEntry>, ReadError> {
        let idx = self.lookup(folded).ok_or(ReadError::NotFound)?;
        if self.node_kind(idx).ok_or(ReadError::NotFound)? != NodeKind::Dir {
            return Err(ReadError::NotADirectory);
        }
        let base = self.node_base(idx).ok_or(ReadError::NotFound)?;
        let first = read_u32(self.bytes, base + N_CHILD_FIRST).ok_or(ReadError::NotFound)?;
        let count = read_u32(self.bytes, base + N_CHILD_COUNT).ok_or(ReadError::NotFound)?;
        let mut out = Vec::new();
        for k in 0..count {
            let cidx = match first.checked_add(k) {
                Some(c) if c < self.child_count => c,
                _ => break,
            };
            let cbase = self.children_off + cidx as usize * CHILD_SIZE;
            let node = match read_u32(self.bytes, cbase + C_NODE) {
                Some(n) => n,
                None => break,
            };
            let (name, kind, size, mtime) = match (self.node_name(node), self.node_kind(node)) {
                (Some(name), Some(kind)) => {
                    let nb = self.node_base(node).unwrap();
                    let (size, mtime) = match kind {
                        NodeKind::Dir => (0, 0),
                        NodeKind::File => (
                            read_u64(self.bytes, nb + N_SIZE).unwrap_or(0),
                            read_i64(self.bytes, nb + N_MTIME).unwrap_or(0),
                        ),
                    };
                    (name, kind, size, mtime)
                }
                _ => break,
            };
            out.push(SnapDirEntry { name, kind, size, mtime });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::SnapshotBuilder;

    // Fixture: /  ->  data(dir) -> {A.esp(file,10), b.esp(file,20), sub(dir)->{c.txt(file,30)}}
    fn fixture() -> Vec<u8> {
        let mut b = SnapshotBuilder::new();
        let c = b.add_file("c.txt", b"src/c", 30, 3, 1, [1; 32]);
        let sub = b.add_dir("sub", &[("c.txt".into(), c)]);
        let a = b.add_file("A.esp", b"src/a", 10, 1, 0, [0; 32]);
        let bb = b.add_file("b.esp", b"src/b", 20, 2, 2, [2; 32]);
        // folded names lowercased (caller's fold; here ASCII lowercase)
        let data = b.add_dir(
            "data",
            &[("a.esp".into(), a), ("b.esp".into(), bb), ("sub".into(), sub)],
        );
        let root = b.add_dir("", &[("data".into(), data)]);
        b.set_root(root);
        b.finish()
    }

    #[test]
    fn getattr_root_and_file() {
        let img = fixture();
        let r = SnapshotReader::open(&img).unwrap();
        assert_eq!(r.getattr(&[]).unwrap().kind, NodeKind::Dir);
        assert_eq!(
            r.getattr(&["data", "a.esp"]).unwrap(),
            SnapStat { kind: NodeKind::File, size: 10, mtime: 1 }
        );
        assert_eq!(r.getattr(&["data", "sub", "c.txt"]).unwrap().size, 30);
        assert_eq!(r.getattr(&["data", "missing"]), None);
    }

    #[test]
    fn resolve_file_carries_source_and_key() {
        let img = fixture();
        let r = SnapshotReader::open(&img).unwrap();
        match r.resolve(&["data", "b.esp"]) {
            SnapResolution::File { source, size, layer, cache_key, .. } => {
                assert_eq!(source, b"src/b");
                assert_eq!(size, 20);
                assert_eq!(layer, 2);
                assert_eq!(cache_key, [2; 32]);
            }
            other => panic!("expected file, got {other:?}"),
        }
        assert_eq!(r.resolve(&["data"]), SnapResolution::Dir);
        assert_eq!(r.resolve(&["nope"]), SnapResolution::NotFound);
    }

    #[test]
    fn readdir_is_case_insensitively_ordered() {
        let img = fixture();
        let r = SnapshotReader::open(&img).unwrap();
        let names: Vec<String> =
            r.readdir(&["data"]).unwrap().into_iter().map(|e| e.name).collect();
        // display names preserved; order follows folded sort: a.esp, b.esp, sub
        assert_eq!(names, vec!["A.esp", "b.esp", "sub"]);
    }

    #[test]
    fn readdir_on_file_errors() {
        let img = fixture();
        let r = SnapshotReader::open(&img).unwrap();
        assert_eq!(r.readdir(&["data", "a.esp"]), Err(ReadError::NotADirectory));
        assert_eq!(r.readdir(&["nope"]), Err(ReadError::NotFound));
    }

    #[test]
    fn open_rejects_bad_magic() {
        let mut img = fixture();
        img[0] ^= 0xFF;
        assert_eq!(SnapshotReader::open(&img).unwrap_err(), LayoutError::BadMagic);
    }
}
```

- [ ] **Step 2: Run tests to verify they compile-then-pass**

Run: `cargo test -p vfs-shared reader`
Expected: PASS (5 tests). If any fails, STOP and report the exact failing assertion and output.

- [ ] **Step 3: Add re-exports in `lib.rs`**

Add:
```rust
pub use reader::{
    LayoutError, NodeKind, ReadError, SnapDirEntry, SnapResolution, SnapStat, SnapshotReader,
};
```
Run: `cargo build -p vfs-shared`
Expected: compiles.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vfs-shared reader`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-shared/src/reader.rs crates/vfs-shared/src/lib.rs
git commit -m "feat(vfs-shared): bounds-checked snapshot reader"
```

---

### Task 5: Reader robustness (torn/corrupt buffers never panic)

**Files:**
- Create: `crates/vfs-shared/tests/robustness.rs`

**Interfaces:**
- Consumes: `SnapshotReader`, `SnapshotBuilder`.
- Produces: an integration test proving no panic on malformed input.

- [ ] **Step 1: Write the test**

Create `crates/vfs-shared/tests/robustness.rs`:

```rust
use vfs_shared::{SnapshotBuilder, SnapshotReader};

fn fixture() -> Vec<u8> {
    let mut b = SnapshotBuilder::new();
    let a = b.add_file("a.esp", b"src/a", 10, 1, 0, [0; 32]);
    let root = b.add_dir("", &[("a.esp".into(), a)]);
    b.set_root(root);
    b.finish()
}

#[test]
fn truncated_buffers_do_not_panic() {
    let img = fixture();
    for len in 0..img.len() {
        let slice = &img[..len];
        // open may fail; if it succeeds, queries must not panic.
        if let Ok(r) = SnapshotReader::open(slice) {
            let _ = r.getattr(&["a.esp"]);
            let _ = r.resolve(&["a.esp"]);
            let _ = r.readdir(&[]);
        }
    }
}

#[test]
fn single_byte_corruption_never_panics() {
    let base = fixture();
    for i in 0..base.len() {
        for bit in 0..8u8 {
            let mut img = base.clone();
            img[i] ^= 1 << bit;
            if let Ok(r) = SnapshotReader::open(&img) {
                // Navigate a few paths; any bounds error must degrade to None/Err.
                let _ = r.getattr(&[]);
                let _ = r.getattr(&["a.esp"]);
                let _ = r.resolve(&["a.esp"]);
                let _ = r.readdir(&[]);
                let _ = r.readdir(&["a.esp"]);
            }
        }
    }
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p vfs-shared --test robustness`
Expected: PASS (2 tests), no panic. If it panics, that's a real bounds bug in the reader — STOP and report the panicking input (index `i`, bit).

- [ ] **Step 3: Commit**

```bash
git add crates/vfs-shared/tests/robustness.rs
git commit -m "test(vfs-shared): reader never panics on torn/corrupt buffers"
```

---

### Task 6: Seqlock — `AlignedBuf`, `publish`, `read_stable` (`seqlock.rs`)

**Files:**
- Modify: `crates/vfs-shared/src/seqlock.rs`
- Modify: `crates/vfs-shared/src/lib.rs` (add seqlock re-exports)

**Interfaces:**
- Consumes: `layout::*`, `reader::SnapshotReader`.
- Produces: `AlignedBuf`, `PublishError`, `publish`, `read_stable`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/vfs-shared/src/seqlock.rs`:

```rust
use core::sync::atomic::{AtomicU64, Ordering};

use crate::layout::{H_GENERATION, H_MAGIC, HEADER_SIZE, MAGIC, read_u32};
use crate::reader::SnapshotReader;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishError {
    ImageTooLarge,
    BadImage,
    Misaligned,
}

/// A heap buffer whose exposed bytes start at an 8-byte-aligned address, so the
/// generation field (Header offset 8) is 8-aligned for atomic access. No unsafe.
pub struct AlignedBuf {
    raw: Vec<u8>,
    off: usize,
    len: usize,
}

impl AlignedBuf {
    pub fn new(len: usize) -> Self {
        // Over-allocate by 8 and expose an 8-aligned subslice. `raw`'s buffer
        // address is stable (no reallocation follows), so `off` stays valid.
        let raw = vec![0u8; len + 8];
        let off = (8 - (raw.as_ptr() as usize % 8)) % 8;
        AlignedBuf { raw, off, len }
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.raw[self.off..self.off + self.len]
    }
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.raw[self.off..self.off + self.len]
    }
}

fn is_aligned(b: &[u8]) -> bool {
    (b.as_ptr() as usize) % 8 == 0
}

/// SAFETY-bearing helper: view the generation slot as an `&AtomicU64`.
#[allow(unsafe_code)]
fn generation(b: &[u8]) -> &AtomicU64 {
    debug_assert!(b.len() >= H_GENERATION + 8);
    debug_assert!(is_aligned(b));
    let ptr = b[H_GENERATION..H_GENERATION + 8].as_ptr() as *const AtomicU64;
    // SAFETY: the generation slot is in-bounds (callers validate len) and
    // 8-aligned (callers validate alignment). AtomicU64 has the same layout as
    // u64. Concurrent atomic access across threads/processes is the intended use
    // of this shared region; no non-atomic access to these 8 bytes occurs.
    unsafe { &*ptr }
}

/// Publish `image` into `shared` under the seqlock. See module docs.
pub fn publish(shared: &mut [u8], image: &[u8]) -> Result<(), PublishError> {
    if image.len() < HEADER_SIZE || read_u32(image, H_MAGIC) != Some(MAGIC) {
        return Err(PublishError::BadImage);
    }
    if image.len() > shared.len() {
        return Err(PublishError::ImageTooLarge);
    }
    if !is_aligned(shared) {
        return Err(PublishError::Misaligned);
    }
    let cur = generation(shared).load(Ordering::Relaxed);
    let odd = cur | 1;
    generation(shared).store(odd, Ordering::Release);
    // Copy everything except the 8-byte generation slot.
    shared[..H_GENERATION].copy_from_slice(&image[..H_GENERATION]);
    shared[H_GENERATION + 8..image.len()].copy_from_slice(&image[H_GENERATION + 8..image.len()]);
    let next_even = (odd + 1) & !1;
    generation(shared).store(next_even, Ordering::Release);
    Ok(())
}

/// Read `shared` under the seqlock, retrying across an overlapping publish.
/// Returns `None` if the buffer is misaligned or holds no valid snapshot.
pub fn read_stable<T>(shared: &[u8], f: impl Fn(&SnapshotReader) -> T) -> Option<T> {
    if !is_aligned(shared) || shared.len() < HEADER_SIZE {
        return None;
    }
    let gen = generation(shared);
    loop {
        let g1 = gen.load(Ordering::Acquire);
        if g1 & 1 != 0 {
            core::hint::spin_loop();
            continue;
        }
        let reader = match SnapshotReader::open(shared) {
            Ok(r) => r,
            Err(_) => {
                // No valid snapshot: distinguish "none" from "mid-publish".
                return if gen.load(Ordering::Acquire) == g1 { None } else { continue };
            }
        };
        let val = f(&reader);
        if gen.load(Ordering::Acquire) == g1 {
            return Some(val);
        }
        // else a publish overlapped; retry.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::SnapshotBuilder;
    use crate::reader::SnapResolution;

    fn image() -> Vec<u8> {
        let mut b = SnapshotBuilder::new();
        let a = b.add_file("a.esp", b"src/a", 10, 1, 0, [0; 32]);
        let root = b.add_dir("", &[("a.esp".into(), a)]);
        b.set_root(root);
        b.finish()
    }

    #[test]
    fn publish_then_read_stable() {
        let img = image();
        let mut buf = AlignedBuf::new(img.len() + 64);
        publish(buf.as_bytes_mut(), &img).unwrap();
        // generation is even after publish
        let g = read_stable(buf.as_bytes(), |r| r.generation()).unwrap();
        assert_eq!(g % 2, 0);
        assert!(g >= 2);
        // content is readable
        let res = read_stable(buf.as_bytes(), |r| r.resolve(&["a.esp"])).unwrap();
        assert!(matches!(res, SnapResolution::File { .. }));
    }

    #[test]
    fn misaligned_publish_errors() {
        let img = image();
        // Force a misaligned slice by offsetting into an aligned buffer by 1.
        let mut buf = AlignedBuf::new(img.len() + 64);
        let bytes = buf.as_bytes_mut();
        let err = publish(&mut bytes[1..], &img).unwrap_err();
        assert_eq!(err, PublishError::Misaligned);
    }

    #[test]
    fn image_too_large_errors() {
        let img = image();
        let mut buf = AlignedBuf::new(img.len() - 1);
        assert_eq!(
            publish(buf.as_bytes_mut(), &img).unwrap_err(),
            PublishError::ImageTooLarge
        );
    }

    #[test]
    fn read_stable_on_empty_buffer_is_none() {
        let buf = AlignedBuf::new(128); // all zeros → bad magic
        assert!(read_stable(buf.as_bytes(), |r| r.root()).is_none());
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p vfs-shared seqlock`
Expected: PASS (4 tests). Note: this task introduces the crate's single `#[allow(unsafe_code)]`; `cargo build` must otherwise remain `deny(unsafe_code)`-clean. If the build reports an `unsafe` denial anywhere else, STOP and report.

- [ ] **Step 3: Add re-exports in `lib.rs`**

Add: `pub use seqlock::{publish, read_stable, AlignedBuf, PublishError};`
Run: `cargo build -p vfs-shared`
Expected: compiles.

- [ ] **Step 4: Run the full crate test suite**

Run: `cargo test -p vfs-shared`
Expected: PASS (all unit + robustness tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-shared/src/seqlock.rs crates/vfs-shared/src/lib.rs
git commit -m "feat(vfs-shared): seqlock publish/read_stable + AlignedBuf"
```

---

### Task 7: Seqlock concurrency test

**Files:**
- Create: `crates/vfs-shared/tests/seqlock_concurrency.rs`

**Interfaces:**
- Consumes: `publish`, `read_stable`, `AlignedBuf`, `SnapshotBuilder`, `SnapshotReader`.
- Produces: a threaded writer/reader test proving readers always see a self-consistent snapshot.

- [ ] **Step 1: Write the test**

Create `crates/vfs-shared/tests/seqlock_concurrency.rs`:

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use vfs_shared::{publish, read_stable, AlignedBuf, SnapshotBuilder};

// A raw pointer wrapper so the writer (needs &mut) and readers (need &) can share
// one buffer across threads — mirroring the cross-process shared-memory reality,
// where Rust's aliasing rules don't apply. All access goes through the seqlock.
#[derive(Clone, Copy)]
struct Shared(*mut u8, usize);
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

fn image(size: u64) -> Vec<u8> {
    let mut b = SnapshotBuilder::new();
    let a = b.add_file("a.esp", b"src/a", size, 1, 0, [0; 32]);
    let root = b.add_dir("", &[("a.esp".into(), a)]);
    b.set_root(root);
    b.finish()
}

#[test]
fn readers_never_see_a_torn_snapshot() {
    let img_a = image(10);
    let img_b = image(9999);
    let cap = img_a.len().max(img_b.len()) + 64;

    let mut buf = AlignedBuf::new(cap);
    publish(buf.as_bytes_mut(), &img_a).unwrap();

    let ptr = Shared(buf.as_bytes_mut().as_mut_ptr(), cap);
    let stop = Arc::new(AtomicBool::new(false));

    // Writer: alternate publishing A and B.
    let writer = {
        let stop = stop.clone();
        thread::spawn(move || {
            #[allow(unsafe_code)]
            let shared = unsafe { std::slice::from_raw_parts_mut(ptr.0, ptr.1) };
            let mut toggle = false;
            while !stop.load(Ordering::Relaxed) {
                let img = if toggle { &img_a } else { &img_b };
                publish(shared, img).unwrap();
                toggle = !toggle;
            }
        })
    };

    // Readers: each read must observe a size that is exactly one of the two
    // published values — never a torn mixture.
    let mut readers = Vec::new();
    for _ in 0..4 {
        let stop = stop.clone();
        readers.push(thread::spawn(move || {
            #[allow(unsafe_code)]
            let shared = unsafe { std::slice::from_raw_parts(ptr.0 as *const u8, ptr.1) };
            for _ in 0..20_000 {
                if let Some(sz) = read_stable(shared, |r| {
                    r.getattr(&["a.esp"]).map(|s| s.size)
                }) {
                    if let Some(sz) = sz {
                        assert!(sz == 10 || sz == 9999, "torn read: size={sz}");
                    }
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        }));
    }

    for r in readers {
        r.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
}
```

Note: the two `#[allow(unsafe_code)]` uses are in the **integration test crate**, not in `vfs-shared` itself (integration tests are separate crates and are not bound by the library's `deny`). They model cross-process sharing.

- [ ] **Step 2: Run the test**

Run: `cargo test -p vfs-shared --test seqlock_concurrency`
Expected: PASS. If it reports "torn read", the seqlock is wrong — STOP and report.

- [ ] **Step 3: Commit**

```bash
git add crates/vfs-shared/tests/seqlock_concurrency.rs
git commit -m "test(vfs-shared): seqlock readers never observe torn snapshots"
```

---

### Task 8: `vfs-core` read-only walk API

**Files:**
- Modify: `crates/vfs-core/src/tree.rs`
- Modify: `crates/vfs-core/src/lib.rs` (export the walk types if needed)

**Interfaces:**
- Consumes: `vfs-core` internals.
- Produces: `VfsTree::walk_postorder(&self, visit: impl FnMut(WalkNode))` yielding, per node, an id, kind, display name, folded name, metadata, source, cache_key, and children (as `(folded_name, child_id)`), in **post-order** (children before parents). Plus `VfsTree::root_id() -> u32`.

**Note:** This is additive and read-only; it does not change any existing behavior. Reuse `vfs-core`'s existing `fold` (from `casefold`) for folded names so they match what the tree used internally.

- [ ] **Step 1: Write the failing test**

Add to the existing `#[cfg(test)] mod tests` in `crates/vfs-core/src/tree.rs`:

```rust
    #[test]
    fn walk_postorder_visits_children_before_parents_with_folded_names() {
        use crate::tree::WalkNodeKind;
        let t = build(vec![layer(0, vec![
            file("Data/A.esp", "src/a", 10, 1),
            file("Data/sub/c.txt", "src/c", 30, 3),
        ])])
        .unwrap();

        let mut order: Vec<String> = Vec::new();
        let mut seen_folded: Vec<String> = Vec::new();
        t.walk_postorder(|n| {
            order.push(n.display.to_string());
            for (folded, _child) in n.children {
                seen_folded.push(folded.clone());
            }
            // A file must carry its source + metadata.
            if let WalkNodeKind::File { source, size, .. } = &n.kind {
                assert!(!source.is_empty());
                assert!(*size > 0);
            }
        });

        // Post-order: a leaf appears before its parent dir.
        let pos = |name: &str| order.iter().position(|x| x == name).unwrap();
        assert!(pos("A.esp") < pos("Data"));
        assert!(pos("c.txt") < pos("sub"));
        assert!(pos("sub") < pos("Data"));
        // Folded child names are lowercased.
        assert!(seen_folded.iter().any(|f| f == "a.esp"));
        assert!(seen_folded.iter().any(|f| f == "sub"));
    }
```

- [ ] **Step 2: Run the test to verify it fails to compile**

Run: `cargo test -p vfs-core walk_postorder`
Expected: FAIL to compile (`walk_postorder`/`WalkNode`/`WalkNodeKind` not found).

- [ ] **Step 3: Implement the walk API**

Add to `crates/vfs-core/src/tree.rs` (public types + method). The tree stores nodes in `self.nodes` with `NodeEntry::{Dir,File}`; expose a read-only view:

```rust
/// Kind + payload for a node visited by `walk_postorder`.
pub enum WalkNodeKind<'a> {
    Dir,
    File {
        source: &'a [u8],
        size: u64,
        mtime: i64,
        layer: LayerId,
        cache_key: crate::model::CacheKey,
    },
}

/// A node handed to the `walk_postorder` visitor.
pub struct WalkNode<'a> {
    pub id: u32,
    pub display: &'a str,
    /// Folded name (empty for the root). Folded with vfs-core's canonical fold.
    pub folded: String,
    pub kind: WalkNodeKind<'a>,
    /// (folded_child_name, child_id) pairs, in this dir's stored order.
    pub children: Vec<(String, u32)>,
}

impl VfsTree {
    pub fn root_id(&self) -> u32 {
        0
    }

    /// Visit every node in post-order (children before parents). Read-only.
    pub fn walk_postorder(&self, mut visit: impl FnMut(WalkNode)) {
        self.walk_from(0, "", &mut visit);
    }

    fn walk_from(&self, idx: u32, folded_name: &str, visit: &mut impl FnMut(WalkNode)) {
        let node = &self.nodes[idx as usize];
        match &node.entry {
            NodeEntry::Dir(d) => {
                // Recurse into children first (post-order).
                for (folded, &child) in &d.children {
                    self.walk_from(child, folded, visit);
                }
                let children: Vec<(String, u32)> =
                    d.children.iter().map(|(f, &c)| (f.clone(), c)).collect();
                visit(WalkNode {
                    id: idx,
                    display: &node.name,
                    folded: folded_name.to_string(),
                    kind: WalkNodeKind::Dir,
                    children,
                });
            }
            NodeEntry::File(f) => {
                visit(WalkNode {
                    id: idx,
                    display: &node.name,
                    folded: folded_name.to_string(),
                    kind: WalkNodeKind::File {
                        source: &f.source.0,
                        size: f.size,
                        mtime: f.mtime,
                        layer: f.layer,
                        cache_key: compute_cache_key(&f.source, f.size, f.mtime),
                    },
                    children: Vec::new(),
                });
            }
        }
    }
}
```

Then export the walk types from `crates/vfs-core/src/lib.rs`:
```rust
pub use tree::{WalkNode, WalkNodeKind};
```

Note: `d.children` is a `BTreeMap<String, u32>` keyed by folded name (from the vfs-core plan), so the key IS the folded name — reuse it directly.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p vfs-core`
Expected: PASS (all vfs-core tests including the new walk test).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-core/src/tree.rs crates/vfs-core/src/lib.rs
git commit -m "feat(vfs-core): read-only post-order walk API for snapshot bridge"
```

---

### Task 9: The `bridge` (feature-gated flatten)

**Files:**
- Modify: `crates/vfs-shared/src/bridge.rs`

**Interfaces:**
- Consumes: `vfs_core::VfsTree` + walk API (Task 8), `SnapshotBuilder`.
- Produces: `#[cfg(feature = "bridge")] pub fn bridge::flatten(tree: &vfs_core::VfsTree) -> Vec<u8>`.

- [ ] **Step 1: Write the failing test**

Replace `crates/vfs-shared/src/bridge.rs` contents with:

```rust
//! Feature-gated bridge from a vfs-core VfsTree to a snapshot image.

use crate::builder::SnapshotBuilder;
use vfs_core::{WalkNode, WalkNodeKind};

/// Flatten a merged vfs-core tree into a snapshot image. Post-order walk means
/// each node's children are already built when the parent dir is added.
pub fn flatten(tree: &vfs_core::VfsTree) -> Vec<u8> {
    let mut builder = SnapshotBuilder::new();
    // Map vfs-core node id → snapshot node index as we build bottom-up.
    let mut id_map: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut root_snap: u32 = 0;

    tree.walk_postorder(|n: WalkNode| {
        let snap_idx = match &n.kind {
            WalkNodeKind::File { source, size, mtime, layer, cache_key } => builder.add_file(
                n.display,
                source,
                *size,
                *mtime,
                layer.0,
                cache_key.0,
            ),
            WalkNodeKind::Dir => {
                let children: Vec<(String, u32)> = n
                    .children
                    .iter()
                    .map(|(folded, child_id)| (folded.clone(), id_map[child_id]))
                    .collect();
                builder.add_dir(n.display, &children)
            }
        };
        id_map.insert(n.id, snap_idx);
        if n.id == tree.root_id() {
            root_snap = snap_idx;
        }
    });

    builder.set_root(root_snap);
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};

    fn file(vpath: &str, source: &str, size: u64, mtime: i64) -> InputEntry {
        InputEntry { vpath: vpath.into(), kind: EntryKind::File, source: source.into(), size, mtime }
    }

    #[test]
    fn flatten_produces_readable_snapshot() {
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![file("data/a.esp", "src/a", 10, 1)],
        }])
        .unwrap();
        let img = flatten(&tree);
        let r = crate::reader::SnapshotReader::open(&img).unwrap();
        assert!(matches!(
            r.resolve(&["data", "a.esp"]),
            crate::reader::SnapResolution::File { .. }
        ));
    }
}
```

- [ ] **Step 2: Run the test (with the feature) to verify it fails then passes**

Run: `cargo test -p vfs-shared --features bridge bridge`
Expected: PASS. If `flatten` mismatches the walk API signatures, STOP and report.

- [ ] **Step 3: Confirm the default build still has no vfs-core dependency**

Run: `cargo build -p vfs-shared`
Expected: compiles WITHOUT pulling `vfs-core` (bridge module is `#[cfg(feature = "bridge")]`, off by default).

- [ ] **Step 4: Commit**

```bash
git add crates/vfs-shared/src/bridge.rs
git commit -m "feat(vfs-shared): feature-gated vfs-core -> snapshot bridge"
```

---

### Task 10: End-to-end round-trip vs `vfs-core`

**Files:**
- Create: `crates/vfs-shared/tests/vfs_core_roundtrip.rs`

**Interfaces:**
- Consumes: `vfs-core` (build + resolve/getattr/readdir), `vfs-shared` (bridge + reader).
- Produces: an integration test asserting the snapshot's answers match `vfs-core`'s.

- [ ] **Step 1: Write the test**

Create `crates/vfs-shared/tests/vfs_core_roundtrip.rs`:

```rust
#![cfg(feature = "bridge")]

use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId, Resolution};
use vfs_shared::bridge::flatten;
use vfs_shared::{NodeKind, SnapResolution, SnapshotReader};

fn file(vpath: &str, source: &str, size: u64, mtime: i64) -> InputEntry {
    InputEntry { vpath: vpath.into(), kind: EntryKind::File, source: source.into(), size, mtime }
}
fn tomb(vpath: &str) -> InputEntry {
    InputEntry { vpath: vpath.into(), kind: EntryKind::Tombstone, source: "".into(), size: 0, mtime: 0 }
}

#[test]
fn snapshot_answers_match_vfs_core() {
    let tree = build(vec![
        Layer {
            id: LayerId(0),
            entries: vec![
                file("Data/Skyrim.esm", "game/Skyrim.esm", 100, 1),
                file("Data/textures/rock.dds", "game/rock.dds", 50, 1),
            ],
        },
        Layer {
            id: LayerId(1),
            entries: vec![file("Data/textures/rock.dds", "mod1/rock.dds", 80, 2)],
        },
        Layer {
            id: LayerId(2),
            entries: vec![file("Data/MyMod.esp", "mod2/MyMod.esp", 10, 3), tomb("Data/Skyrim.esm")],
        },
    ])
    .unwrap();

    let img = flatten(&tree);
    let r = SnapshotReader::open(&img).unwrap();

    // The overridden texture resolves to mod1 in both.
    match (tree.resolve("Data/textures/rock.dds"), r.resolve(&["data", "textures", "rock.dds"])) {
        (
            Resolution::File { source: cs, size: csz, layer: cl, .. },
            SnapResolution::File { source: ss, size: ssz, layer: sl, .. },
        ) => {
            assert_eq!(cs, vfs_core::SourceId::from("mod1/rock.dds"));
            assert_eq!(ss, b"mod1/rock.dds");
            assert_eq!(csz, ssz);
            assert_eq!(cl.0, sl);
        }
        other => panic!("resolution mismatch: {other:?}"),
    }

    // The tombstoned master is gone in both. (Reader keys are folded: "skyrim.esm".)
    assert_eq!(tree.resolve("Data/Skyrim.esm"), Resolution::NotFound);
    assert_eq!(r.resolve(&["data", "skyrim.esm"]), SnapResolution::NotFound);

    // Merged Data listing matches (case-insensitive order, tombstone honored).
    let core_names: Vec<String> =
        tree.readdir("Data", None).unwrap().into_iter().map(|e| e.name).collect();
    let snap_names: Vec<String> =
        r.readdir(&["data"]).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(core_names, snap_names);
    assert_eq!(snap_names, vec!["MyMod.esp", "textures"]);

    // getattr kinds agree for a directory.
    assert_eq!(r.getattr(&["data", "textures"]).unwrap().kind, NodeKind::Dir);
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p vfs-shared --features bridge --test vfs_core_roundtrip`
Expected: PASS. If `core_names != snap_names`, the flatten or reader is losing merge fidelity — STOP and report.

- [ ] **Step 3: Run the whole workspace suite**

Run: `cargo test --workspace` and `cargo test -p vfs-shared --features bridge`
Expected: all PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/vfs-shared/tests/vfs_core_roundtrip.rs
git commit -m "test(vfs-shared): end-to-end snapshot round-trip matches vfs-core"
```

---

## Self-review

**Spec coverage:**
- §3 layout (Header/SnapNode/SnapChild, offsets, asserts) → Task 2. ✓
- §4 builder → Task 3. ✓
- §5 reader (open/getattr/resolve/readdir, bounds-checked) → Task 4; torn-read safety → Task 5. ✓
- §5 seqlock (publish/read_stable, alignment, one audited unsafe) → Task 6; concurrency proof → Task 7. ✓
- §5 `AlignedBuf` (no-unsafe aligned buffer) → Task 6. ✓
- §6 fold contract (fold-free reader; bridge folds via vfs-core) → Tasks 4, 8, 9. ✓
- §7 error types (LayoutError/ReadError/PublishError incl. Misaligned) → Tasks 4, 6. ✓
- §7 walk API on vfs-core → Task 8. ✓
- §5/§7 bridge (feature-gated) + round-trip vs vfs-core → Tasks 9, 10. ✓
- §3 D0/G9 layout asserts → Task 2 (compile-time). ✓
- §8 deps/toolchain/features (default no-dep, `bridge`→vfs-core, dev-dep) → Task 1. ✓

**Deferred by spec (correctly absent):** cache index/refcounts, ring/arena/sync regions, OS mapping, reclamation/RCU, in-crate fold, runtime mutation.

**Placeholder scan:** none. Every code step is complete. The single `#[allow(unsafe_code)]` (Task 6) and the test-crate `unsafe` for pointer sharing (Task 7) are explicit and `// SAFETY:`-documented, not placeholders.

**Type consistency:** `Header`/`SnapNode`/`SnapChild` field names and offset consts are used identically in `layout.rs` (Task 2), `builder.rs` (Task 3), `reader.rs` (Task 4), `seqlock.rs` (Task 6). `SnapshotReader`/`SnapResolution`/`NodeKind`/`SnapStat`/`ReadError`/`LayoutError`/`PublishError` names match across reader, seqlock, bridge, and both integration tests. `WalkNode`/`WalkNodeKind` (Task 8) match their use in `bridge::flatten` (Task 9). `LayerId(.0)`, `SourceId::from`, `CacheKey(.0)` usages match the `vfs-core` public API from the slice-1 plan.

*End of plan.*
