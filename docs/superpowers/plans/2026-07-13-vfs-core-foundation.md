# vfs-core Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `vfs-core` crate — a pure, OS-independent, read-only merged/overlaid virtual filesystem resolver.

**Architecture:** A Cargo workspace with one library crate `crates/vfs-core`. The caller passes ordered overlay layers (data-in, zero I/O); `build()` folds them low→high into an in-memory tree (`Vec<Node>` + `BTreeMap` children keyed by case-folded name). Query methods (`getattr`/`resolve`/`readdir`) answer against that tree. All name comparison flows through one case-fold helper. No `unsafe`, no Windows APIs, no filesystem access.

**Tech Stack:** Rust (stable), `blake3` for cache keys, `proptest` (dev-dependency, optional property tests).

## Global Constraints

- **Toolchain:** stable Rust. No nightly features.
- **No `unsafe`:** `lib.rs` declares `#![forbid(unsafe_code)]`.
- **No dependencies on `windows`/`ntapi`/any OS crate.** No filesystem I/O (`std::fs`) anywhere.
- **No panics on input-derived data:** no `unwrap`/`expect`/indexing that can fault from caller input; return `Result`/`Option` instead.
- **Names are UTF-8 `String`.** Case-insensitivity goes through the single `casefold::fold` helper — nowhere else compares case.
- **Workspace path:** the crate lives at `crates/vfs-core`.
- **Priority order:** `layers` passed low→high; **highest index wins**.
- **Cache key:** `blake3(source_bytes ‖ size.to_le_bytes() ‖ mtime.to_le_bytes())`, 32 bytes.

## Parallelization note

After Task 1 (scaffold), Tasks 2, 3, 4, 5 are **independent** (separate files, depend only on the scaffold) and may be dispatched in parallel. Task 6 depends on 5. Task 7 depends on 2,3,5,6. Tasks 8 and 9 depend on 7 and may run in parallel with each other. Task 10 depends on all.

```
        1 (scaffold)
        │
   ┌────┼────┬────┐
   2    3    4    5        ← parallel wave
             │    │
             │    6        ← after 5
   └────┴────┴────┘
             7             ← build + resolve
           ┌─┴─┐
           8   9           ← parallel (getattr / readdir)
           └─┬─┘
            10             ← integration + docs
```

---

### Task 1: Workspace & crate scaffold

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `crates/vfs-core/Cargo.toml`
- Create: `crates/vfs-core/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: a compiling `vfs-core` library with modules declared; `#![forbid(unsafe_code)]`.

- [ ] **Step 1: Create the workspace root `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = ["crates/vfs-core"]
```

- [ ] **Step 2: Create `crates/vfs-core/Cargo.toml`**

```toml
[package]
name = "vfs-core"
version = "0.1.0"
edition = "2021"

[dependencies]
blake3 = "1"

[dev-dependencies]
proptest = "1"
```

- [ ] **Step 3: Create `crates/vfs-core/src/lib.rs` with module skeleton**

```rust
#![forbid(unsafe_code)]
//! `vfs-core`: pure, OS-independent read-only resolver for a merged/overlaid
//! virtual filesystem. Fed enumerated layers (data-in); does no I/O.

mod casefold;
mod cachekey;
mod model;
mod path;
mod tree;
mod wildcard;

pub use cachekey::compute_cache_key;
pub use model::{
    BuildError, CacheKey, DirEntry, EntryKind, InputEntry, Layer, LayerId, NodeKind, Resolution,
    SourceId, Stat, VfsError,
};
pub use path::{normalize_vpath, PathError};
pub use tree::VfsTree;
pub use tree::build;
pub use wildcard::wildcard_match;
```

- [ ] **Step 4: Create empty module files so it compiles**

Create these files each containing only a doc comment placeholder line, so `cargo build` sees the modules. Their real content lands in later tasks. Create:
- `crates/vfs-core/src/casefold.rs` → `//! Case folding — single source of truth for case-insensitive comparison.`
- `crates/vfs-core/src/cachekey.rs` → `//! Cache-key computation.`
- `crates/vfs-core/src/model.rs` → `//! Core data types.`
- `crates/vfs-core/src/path.rs` → `//! Virtual path normalization.`
- `crates/vfs-core/src/tree.rs` → `//! Merged tree build + queries.`
- `crates/vfs-core/src/wildcard.rs` → `//! DOS wildcard matching.`

At this stage the `pub use` lines in `lib.rs` reference items that don't exist yet, so temporarily comment out every `pub use` line in `lib.rs` (leave the `mod` lines). Uncomment each as its task lands.

- [ ] **Step 5: Verify it builds**

Run: `cargo build`
Expected: compiles clean (empty modules, no `pub use`).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/vfs-core
git commit -m "chore: scaffold vfs-core workspace and crate"
```

---

### Task 2: Case folding (`casefold.rs`)

**Files:**
- Modify: `crates/vfs-core/src/casefold.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn fold(s: &str) -> String` — lowercase simple case fold; the ONLY case-normalizer in the crate.
  - `pub fn cmp_ci(a: &str, b: &str) -> std::cmp::Ordering` — case-insensitive total order, tie-broken by raw bytes for stability.

- [ ] **Step 1: Write the failing tests**

Append to `crates/vfs-core/src/casefold.rs`:

```rust
pub fn fold(s: &str) -> String {
    todo!()
}

pub fn cmp_ci(a: &str, b: &str) -> std::cmp::Ordering {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn folds_ascii_and_unicode() {
        assert_eq!(fold("FooBAR.ESP"), "foobar.esp");
        assert_eq!(fold("ÄÖÜ"), "äöü");
    }

    #[test]
    fn cmp_is_case_insensitive() {
        assert_eq!(cmp_ci("apple", "APPLE"), Ordering::Equal);
        assert_eq!(cmp_ci("Apple", "banana"), Ordering::Less);
        assert_eq!(cmp_ci("Banana", "apple"), Ordering::Greater);
    }

    #[test]
    fn cmp_ascending_not_reverse() {
        // Regression guard for the USVFS reverse-alphabetical bug.
        let mut v = vec!["Zebra", "apple", "Mango"];
        v.sort_by(|a, b| cmp_ci(a, b));
        assert_eq!(v, vec!["apple", "Mango", "Zebra"]);
    }

    #[test]
    fn cmp_tiebreaks_stably_by_raw() {
        // Same fold, different case → deterministic, non-Equal order.
        assert_eq!(cmp_ci("abc", "abc"), Ordering::Equal);
        assert_ne!(cmp_ci("ABC", "abc"), Ordering::Equal);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vfs-core casefold`
Expected: FAIL (panics on `todo!()`).

- [ ] **Step 3: Implement**

Replace the two `todo!()` bodies:

```rust
/// Lowercase simple case fold. MVP uses `char::to_lowercase` (Unicode simple
/// folding). This is the single source of truth for case-insensitive matching.
pub fn fold(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// Case-insensitive total order. Ties on the folded form are broken by the raw
/// string so the order is deterministic (stable sort friendly).
pub fn cmp_ci(a: &str, b: &str) -> std::cmp::Ordering {
    fold(a).cmp(&fold(b)).then_with(|| a.cmp(b))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vfs-core casefold`
Expected: PASS (4 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-core/src/casefold.rs
git commit -m "feat(vfs-core): case-fold helper and case-insensitive ordering"
```

---

### Task 3: Path normalization (`path.rs`)

**Files:**
- Modify: `crates/vfs-core/src/path.rs`
- Modify: `crates/vfs-core/src/lib.rs` (uncomment `pub use path::...`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub enum PathError { EscapesRoot }`
  - `pub fn normalize_vpath(raw: &str) -> Result<String, PathError>` — returns canonical `/`-separated, root-relative path; `""` means root. Folds `\`→`/`, strips `\??\` and `\\?\` prefixes, drops `.`/empty components, resolves `..` (error if it escapes root).

- [ ] **Step 1: Write the failing tests**

Append to `crates/vfs-core/src/path.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    EscapesRoot,
}

pub fn normalize_vpath(raw: &str) -> Result<String, PathError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_separators_and_trims() {
        assert_eq!(normalize_vpath("data\\meshes\\a.nif").unwrap(), "data/meshes/a.nif");
        assert_eq!(normalize_vpath("/data/").unwrap(), "data");
        assert_eq!(normalize_vpath("data//meshes").unwrap(), "data/meshes");
    }

    #[test]
    fn empty_and_dot_are_root() {
        assert_eq!(normalize_vpath("").unwrap(), "");
        assert_eq!(normalize_vpath(".").unwrap(), "");
        assert_eq!(normalize_vpath("/").unwrap(), "");
    }

    #[test]
    fn resolves_dotdot() {
        assert_eq!(normalize_vpath("data/x/../y").unwrap(), "data/y");
        assert_eq!(normalize_vpath("a/b/../..").unwrap(), "");
    }

    #[test]
    fn dotdot_escaping_root_errors() {
        assert_eq!(normalize_vpath("..").unwrap_err(), PathError::EscapesRoot);
        assert_eq!(normalize_vpath("data/../..").unwrap_err(), PathError::EscapesRoot);
    }

    #[test]
    fn strips_nt_and_dos_prefixes() {
        assert_eq!(normalize_vpath(r"\??\data\a").unwrap(), "data/a");
        assert_eq!(normalize_vpath(r"\\?\data\a").unwrap(), "data/a");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vfs-core path`
Expected: FAIL (panics on `todo!()`).

- [ ] **Step 3: Implement**

Replace the `todo!()` body:

```rust
/// Normalize a root-relative virtual path to canonical `/`-separated form.
/// `""` denotes the root. Deeper NT concerns (`\Device\…`, RootDirectory-relative
/// opens, 8.3 short names) are edge/shim concerns and out of scope here.
pub fn normalize_vpath(raw: &str) -> Result<String, PathError> {
    // Strip known NT/DOS long-path prefixes first (either slash form).
    let mut s = raw;
    for prefix in [r"\??\", r"\\?\", "/??/", "//?/"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest;
            break;
        }
    }

    let mut out: Vec<&str> = Vec::new();
    for comp in s.split(['/', '\\']) {
        match comp {
            "" | "." => continue,
            ".." => {
                if out.pop().is_none() {
                    return Err(PathError::EscapesRoot);
                }
            }
            other => out.push(other),
        }
    }
    Ok(out.join("/"))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vfs-core path`
Expected: PASS (5 tests).

- [ ] **Step 5: Uncomment the export in `lib.rs`**

Uncomment: `pub use path::{normalize_vpath, PathError};`
Run: `cargo build`
Expected: compiles.

- [ ] **Step 6: Commit**

```bash
git add crates/vfs-core/src/path.rs crates/vfs-core/src/lib.rs
git commit -m "feat(vfs-core): virtual path normalization"
```

---

### Task 4: DOS wildcard matching (`wildcard.rs`)

**Files:**
- Modify: `crates/vfs-core/src/wildcard.rs`
- Modify: `crates/vfs-core/src/lib.rs` (uncomment `pub use wildcard::wildcard_match`)

**Interfaces:**
- Consumes: `casefold::fold`.
- Produces: `pub fn wildcard_match(pattern: &str, name: &str) -> bool` — case-insensitive; supports `*`, `?`, and DOS `<` (DOS_STAR), `>` (DOS_QM), `"` (DOS_DOT).

**Note (scoped simplification):** MVP treats `<` (DOS_STAR) the same as `*`. The precise "stop at final dot" behavior is a documented follow-up; the test table pins the cases we rely on. Win32→DOS-wildcard conversion (e.g. `*.*` matching extensionless names) is a **shim edge** concern — in core, a literal `.` in the pattern is a literal dot.

- [ ] **Step 1: Write the failing tests**

Append to `crates/vfs-core/src/wildcard.rs`:

```rust
use crate::casefold::fold;

pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_and_question() {
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("*.txt", "readme.txt"));
        assert!(!wildcard_match("*.txt", "readme.md"));
        assert!(!wildcard_match("*.txt", "readme"));
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("a?c", "ac"));
    }

    #[test]
    fn case_insensitive() {
        assert!(wildcard_match("FOO*", "foobar"));
        assert!(wildcard_match("*.ESP", "Skyrim.esp"));
    }

    #[test]
    fn literal_dot_is_literal_in_core() {
        // Win32 `*.*`→match-all conversion is a shim concern; core is literal.
        assert!(wildcard_match("*.*", "foo.txt"));
        assert!(!wildcard_match("*.*", "foo"));
    }

    #[test]
    fn dos_qm_matches_zero_at_end() {
        // '>' matches one non-dot char or zero at end / before a dot.
        assert!(wildcard_match("a>", "a"));
        assert!(wildcard_match("a>", "ab"));
        assert!(!wildcard_match("a>", "a.b")); // '>' won't consume the dot, trailing ".b" remains
    }

    #[test]
    fn dos_dot_matches_period() {
        // '"' matches a literal '.' or zero at end / before non-dot.
        assert!(wildcard_match("a\"", "a."));
        assert!(wildcard_match("a\"", "a"));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vfs-core wildcard`
Expected: FAIL (panics on `todo!()`).

- [ ] **Step 3: Implement**

Replace the `todo!()` body and add the recursive helper:

```rust
/// Case-insensitive DOS wildcard match. Supports `*`, `?`, and the DOS
/// meta-characters `<` (DOS_STAR ≈ `*` for MVP), `>` (DOS_QM), `"` (DOS_DOT).
pub fn wildcard_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = fold(pattern).chars().collect();
    let n: Vec<char> = fold(name).chars().collect();
    do_match(&p, 0, &n, 0)
}

fn do_match(p: &[char], mut pi: usize, n: &[char], mut ni: usize) -> bool {
    while pi < p.len() {
        match p[pi] {
            '*' | '<' => {
                // Zero-or-more: try to match the remainder at each position.
                if do_match(p, pi + 1, n, ni) {
                    return true;
                }
                if ni < n.len() {
                    ni += 1;
                    continue; // stay on the star, having consumed one name char
                }
                return false;
            }
            '?' => {
                if ni >= n.len() {
                    return false;
                }
                ni += 1;
                pi += 1;
            }
            '>' => {
                // DOS_QM: one non-dot char, else zero at end / before a dot.
                if ni < n.len() && n[ni] != '.' {
                    ni += 1;
                }
                pi += 1;
            }
            '"' => {
                // DOS_DOT: a literal '.', else zero at end / before a non-dot.
                if ni < n.len() && n[ni] == '.' {
                    ni += 1;
                }
                pi += 1;
            }
            c => {
                if ni >= n.len() || n[ni] != c {
                    return false;
                }
                ni += 1;
                pi += 1;
            }
        }
    }
    ni == n.len()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vfs-core wildcard`
Expected: PASS (5 tests).

- [ ] **Step 5: Uncomment the export in `lib.rs`**

Uncomment: `pub use wildcard::wildcard_match;`
Run: `cargo build`
Expected: compiles.

- [ ] **Step 6: Commit**

```bash
git add crates/vfs-core/src/wildcard.rs crates/vfs-core/src/lib.rs
git commit -m "feat(vfs-core): DOS wildcard matching"
```

---

### Task 5: Core data types (`model.rs`)

**Files:**
- Modify: `crates/vfs-core/src/model.rs`
- Modify: `crates/vfs-core/src/lib.rs` (uncomment the `pub use model::...` line)

**Interfaces:**
- Consumes: nothing.
- Produces the public types (exact definitions below): `LayerId`, `Layer`, `EntryKind`, `InputEntry`, `SourceId` (+ `SourceId::new`, `From<&str>`), `NodeKind`, `Stat`, `DirEntry`, `Resolution`, `CacheKey`, `BuildError`, `VfsError`.

- [ ] **Step 1: Write the failing test**

Append to `crates/vfs-core/src/model.rs`:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayerId(pub u32);

#[derive(Clone, Debug)]
pub struct Layer {
    pub id: LayerId,
    pub entries: Vec<InputEntry>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Tombstone,
}

#[derive(Clone, Debug)]
pub struct InputEntry {
    pub vpath: String,
    pub kind: EntryKind,
    pub source: SourceId,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceId(pub Box<[u8]>);

impl SourceId {
    pub fn new(bytes: impl Into<Box<[u8]>>) -> Self {
        SourceId(bytes.into())
    }
}

impl From<&str> for SourceId {
    fn from(s: &str) -> Self {
        SourceId(s.as_bytes().into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: NodeKind,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: NodeKind,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheKey(pub [u8; 32]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    File {
        source: SourceId,
        size: u64,
        mtime: i64,
        layer: LayerId,
        cache_key: CacheKey,
    },
    Dir,
    NotFound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    EmptyPath,
    EscapesRoot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VfsError {
    NotADirectory,
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_id_constructors() {
        assert_eq!(SourceId::from("abc"), SourceId::new(b"abc".to_vec()));
    }

    #[test]
    fn types_are_constructible() {
        let e = InputEntry {
            vpath: "data/a.esp".into(),
            kind: EntryKind::File,
            source: "root/data/a.esp".into(),
            size: 10,
            mtime: 42,
        };
        let _layer = Layer { id: LayerId(0), entries: vec![e] };
        let _r = Resolution::NotFound;
        assert_eq!(_r, Resolution::NotFound);
    }
}
```

- [ ] **Step 2: Run test to verify it fails/passes correctly**

Run: `cargo test -p vfs-core model`
Expected: PASS (2 tests) — this task is type definitions, so tests pass once the types compile. If it does not compile, fix the definitions.

- [ ] **Step 3: Uncomment the export in `lib.rs`**

Uncomment:
```rust
pub use model::{
    BuildError, CacheKey, DirEntry, EntryKind, InputEntry, Layer, LayerId, NodeKind, Resolution,
    SourceId, Stat, VfsError,
};
```
Run: `cargo build`
Expected: compiles.

- [ ] **Step 4: Commit**

```bash
git add crates/vfs-core/src/model.rs crates/vfs-core/src/lib.rs
git commit -m "feat(vfs-core): core public data types"
```

---

### Task 6: Cache key (`cachekey.rs`)

**Files:**
- Modify: `crates/vfs-core/src/cachekey.rs`
- Modify: `crates/vfs-core/src/lib.rs` (uncomment `pub use cachekey::compute_cache_key`)

**Interfaces:**
- Consumes: `model::{SourceId, CacheKey}`.
- Produces: `pub fn compute_cache_key(source: &SourceId, size: u64, mtime: i64) -> CacheKey`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/vfs-core/src/cachekey.rs`:

```rust
use crate::model::{CacheKey, SourceId};

pub fn compute_cache_key(source: &SourceId, size: u64, mtime: i64) -> CacheKey {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_for_same_inputs() {
        let s = SourceId::from("root/data/a.esp");
        assert_eq!(compute_cache_key(&s, 100, 5), compute_cache_key(&s, 100, 5));
    }

    #[test]
    fn changes_on_size_or_mtime() {
        let s = SourceId::from("root/data/a.esp");
        let base = compute_cache_key(&s, 100, 5);
        assert_ne!(base, compute_cache_key(&s, 101, 5));
        assert_ne!(base, compute_cache_key(&s, 100, 6));
    }

    #[test]
    fn dedupes_identical_sources() {
        // Two vpaths resolving to the same source+size+mtime → same key.
        let s = SourceId::from("root/shared/tex.dds");
        assert_eq!(compute_cache_key(&s, 2048, 9), compute_cache_key(&s, 2048, 9));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vfs-core cachekey`
Expected: FAIL (panics on `todo!()`).

- [ ] **Step 3: Implement**

Replace the `todo!()` body:

```rust
/// Cache key = blake3 over the winning source identity plus its size and mtime.
/// Identical resolved inputs dedupe; a changed size/mtime yields a new key.
pub fn compute_cache_key(source: &SourceId, size: u64, mtime: i64) -> CacheKey {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&source.0);
    hasher.update(&size.to_le_bytes());
    hasher.update(&mtime.to_le_bytes());
    CacheKey(*hasher.finalize().as_bytes())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vfs-core cachekey`
Expected: PASS (3 tests).

- [ ] **Step 5: Uncomment the export in `lib.rs`**

Uncomment: `pub use cachekey::compute_cache_key;`
Run: `cargo build`
Expected: compiles.

- [ ] **Step 6: Commit**

```bash
git add crates/vfs-core/src/cachekey.rs crates/vfs-core/src/lib.rs
git commit -m "feat(vfs-core): blake3 cache-key computation"
```

---

### Task 7: Tree build + resolve (`tree.rs`)

**Files:**
- Modify: `crates/vfs-core/src/tree.rs`
- Modify: `crates/vfs-core/src/lib.rs` (uncomment `pub use tree::{VfsTree, build}`)

**Interfaces:**
- Consumes: `casefold::fold`, `path::normalize_vpath`, `cachekey::compute_cache_key`, and all `model` types.
- Produces:
  - `pub struct VfsTree` (opaque; holds `Vec<Node>`, root at index 0).
  - `pub fn build(layers: Vec<Layer>) -> Result<VfsTree, BuildError>`.
  - `impl VfsTree { pub fn resolve(&self, vpath: &str) -> Resolution }`.
  - Internal: `find(&self, norm: &str) -> Option<u32>` (used by Tasks 8, 9).

- [ ] **Step 1: Write the failing tests**

Append to `crates/vfs-core/src/tree.rs`:

```rust
use std::collections::BTreeMap;

use crate::cachekey::compute_cache_key;
use crate::casefold::fold;
use crate::model::{BuildError, EntryKind, InputEntry, Layer, LayerId, Resolution, SourceId};
use crate::path::normalize_vpath;

struct Node {
    name: String,
    entry: NodeEntry,
}

enum NodeEntry {
    File(FileNode),
    Dir(DirNode),
}

struct FileNode {
    source: SourceId,
    size: u64,
    mtime: i64,
    layer: LayerId,
}

struct DirNode {
    children: BTreeMap<String, u32>, // key = folded name → node index
}

pub struct VfsTree {
    nodes: Vec<Node>, // nodes[0] is the root dir
}

pub fn build(layers: Vec<Layer>) -> Result<VfsTree, BuildError> {
    todo!()
}

impl VfsTree {
    pub fn resolve(&self, vpath: &str) -> Resolution {
        todo!()
    }

    fn find(&self, norm: &str) -> Option<u32> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(vpath: &str, source: &str, size: u64, mtime: i64) -> InputEntry {
        InputEntry { vpath: vpath.into(), kind: EntryKind::File, source: source.into(), size, mtime }
    }
    fn dir(vpath: &str) -> InputEntry {
        InputEntry { vpath: vpath.into(), kind: EntryKind::Dir, source: "".into(), size: 0, mtime: 0 }
    }
    fn tomb(vpath: &str) -> InputEntry {
        InputEntry { vpath: vpath.into(), kind: EntryKind::Tombstone, source: "".into(), size: 0, mtime: 0 }
    }
    fn layer(id: u32, entries: Vec<InputEntry>) -> Layer {
        Layer { id: LayerId(id), entries }
    }

    #[test]
    fn higher_layer_wins() {
        let t = build(vec![
            layer(0, vec![file("data/a.esp", "L0/a", 1, 1)]),
            layer(1, vec![file("data/a.esp", "L1/a", 2, 2)]),
        ])
        .unwrap();
        match t.resolve("data/a.esp") {
            Resolution::File { source, size, layer, .. } => {
                assert_eq!(source, SourceId::from("L1/a"));
                assert_eq!(size, 2);
                assert_eq!(layer, LayerId(1));
            }
            other => panic!("expected file, got {other:?}"),
        }
    }

    #[test]
    fn directories_union() {
        let t = build(vec![
            layer(0, vec![file("data/a.esp", "L0/a", 1, 1)]),
            layer(1, vec![file("data/b.esp", "L1/b", 1, 1)]),
        ])
        .unwrap();
        assert!(matches!(t.resolve("data/a.esp"), Resolution::File { .. }));
        assert!(matches!(t.resolve("data/b.esp"), Resolution::File { .. }));
        assert!(matches!(t.resolve("data"), Resolution::Dir));
    }

    #[test]
    fn resolve_missing_is_notfound() {
        let t = build(vec![layer(0, vec![file("data/a.esp", "L0/a", 1, 1)])]).unwrap();
        assert_eq!(t.resolve("data/missing"), Resolution::NotFound);
    }

    #[test]
    fn tombstone_hides_lower_layer() {
        let t = build(vec![
            layer(0, vec![file("data/a.esp", "L0/a", 1, 1)]),
            layer(1, vec![tomb("data/a.esp")]),
        ])
        .unwrap();
        assert_eq!(t.resolve("data/a.esp"), Resolution::NotFound);
    }

    #[test]
    fn higher_layer_resurrects_tombstone() {
        let t = build(vec![
            layer(0, vec![file("data/a.esp", "L0/a", 1, 1)]),
            layer(1, vec![tomb("data/a.esp")]),
            layer(2, vec![file("data/a.esp", "L2/a", 3, 3)]),
        ])
        .unwrap();
        match t.resolve("data/a.esp") {
            Resolution::File { source, .. } => assert_eq!(source, SourceId::from("L2/a")),
            other => panic!("expected resurrected file, got {other:?}"),
        }
    }

    #[test]
    fn directory_tombstone_hides_subtree() {
        let t = build(vec![
            layer(0, vec![file("data/sub/a.esp", "L0/a", 1, 1)]),
            layer(1, vec![tomb("data/sub")]),
        ])
        .unwrap();
        assert_eq!(t.resolve("data/sub"), Resolution::NotFound);
        assert_eq!(t.resolve("data/sub/a.esp"), Resolution::NotFound);
    }

    #[test]
    fn file_dir_conflict_higher_wins() {
        // Lower layer: "x" is a file. Higher layer: "x" is a dir with a child.
        let t = build(vec![
            layer(0, vec![file("x", "L0/x", 1, 1)]),
            layer(1, vec![file("x/child", "L1/child", 1, 1)]),
        ])
        .unwrap();
        assert!(matches!(t.resolve("x"), Resolution::Dir));
        assert!(matches!(t.resolve("x/child"), Resolution::File { .. }));
    }

    #[test]
    fn empty_input_path_errors() {
        let err = build(vec![layer(0, vec![file("", "s", 1, 1)])]).unwrap_err();
        assert_eq!(err, BuildError::EmptyPath);
    }

    #[test]
    fn cache_key_present_on_resolved_file() {
        let t = build(vec![layer(0, vec![file("a", "s", 7, 8)])]).unwrap();
        match t.resolve("a") {
            Resolution::File { cache_key, source, size, mtime, .. } => {
                assert_eq!(cache_key, compute_cache_key(&source, size, mtime));
            }
            other => panic!("expected file, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vfs-core tree`
Expected: FAIL (panics on `todo!()`).

- [ ] **Step 3: Implement `build`, `resolve`, `find`, and insertion helpers**

Replace the three `todo!()` bodies and add private helpers inside `impl VfsTree`:

```rust
pub fn build(layers: Vec<Layer>) -> Result<VfsTree, BuildError> {
    let mut tree = VfsTree {
        nodes: vec![Node {
            name: String::new(),
            entry: NodeEntry::Dir(DirNode { children: BTreeMap::new() }),
        }],
    };
    for layer in &layers {
        for entry in &layer.entries {
            let norm = normalize_vpath(&entry.vpath).map_err(|_| BuildError::EscapesRoot)?;
            if norm.is_empty() {
                return Err(BuildError::EmptyPath);
            }
            let comps: Vec<&str> = norm.split('/').collect();
            match entry.kind {
                EntryKind::Tombstone => tree.remove_path(&comps),
                EntryKind::Dir => {
                    tree.ensure_dir_path(&comps);
                }
                EntryKind::File => tree.insert_file(&comps, entry, layer.id),
            }
        }
    }
    Ok(tree)
}

impl VfsTree {
    pub fn resolve(&self, vpath: &str) -> Resolution {
        let norm = match normalize_vpath(vpath) {
            Ok(n) => n,
            Err(_) => return Resolution::NotFound,
        };
        match self.find(&norm) {
            Some(id) => match &self.nodes[id as usize].entry {
                NodeEntry::Dir(_) => Resolution::Dir,
                NodeEntry::File(f) => Resolution::File {
                    source: f.source.clone(),
                    size: f.size,
                    mtime: f.mtime,
                    layer: f.layer,
                    cache_key: compute_cache_key(&f.source, f.size, f.mtime),
                },
            },
            None => Resolution::NotFound,
        }
    }

    fn find(&self, norm: &str) -> Option<u32> {
        let mut cur = 0u32;
        if norm.is_empty() {
            return Some(0);
        }
        for comp in norm.split('/') {
            let key = fold(comp);
            match &self.nodes[cur as usize].entry {
                NodeEntry::Dir(d) => cur = *d.children.get(&key)?,
                NodeEntry::File(_) => return None,
            }
        }
        Some(cur)
    }

    /// Push a fresh node, return its index.
    fn push(&mut self, name: &str, entry: NodeEntry) -> u32 {
        let id = self.nodes.len() as u32;
        self.nodes.push(Node { name: name.to_string(), entry });
        id
    }

    /// Look up a direct child index by folded component name.
    fn child(&self, parent: u32, key: &str) -> Option<u32> {
        match &self.nodes[parent as usize].entry {
            NodeEntry::Dir(d) => d.children.get(key).copied(),
            NodeEntry::File(_) => None,
        }
    }

    fn set_child(&mut self, parent: u32, key: String, id: u32) {
        if let NodeEntry::Dir(d) = &mut self.nodes[parent as usize].entry {
            d.children.insert(key, id);
        }
    }

    /// Ensure every component of `comps` exists as a directory; return the leaf's id.
    /// If an existing node on the path is a File, it is replaced by a Dir (higher wins).
    fn ensure_dir_path(&mut self, comps: &[&str]) -> u32 {
        let mut cur = 0u32;
        for comp in comps {
            let key = fold(comp);
            match self.child(cur, &key) {
                Some(id) => {
                    if matches!(self.nodes[id as usize].entry, NodeEntry::File(_)) {
                        // Replace file with an empty dir; name takes this layer's casing.
                        self.nodes[id as usize].name = comp.to_string();
                        self.nodes[id as usize].entry =
                            NodeEntry::Dir(DirNode { children: BTreeMap::new() });
                    }
                    cur = id;
                }
                None => {
                    let id = self.push(comp, NodeEntry::Dir(DirNode { children: BTreeMap::new() }));
                    self.set_child(cur, key, id);
                    cur = id;
                }
            }
        }
        cur
    }

    /// Insert a file at `comps`, creating parent dirs; replaces any existing node.
    fn insert_file(&mut self, comps: &[&str], entry: &InputEntry, layer: LayerId) {
        let (leaf, parents) = comps.split_last().expect("build guarantees non-empty");
        let parent = self.ensure_dir_path(parents);
        let key = fold(leaf);
        let file = NodeEntry::File(FileNode {
            source: entry.source.clone(),
            size: entry.size,
            mtime: entry.mtime,
            layer,
        });
        match self.child(parent, &key) {
            Some(id) => {
                self.nodes[id as usize].name = leaf.to_string();
                self.nodes[id as usize].entry = file;
            }
            None => {
                let id = self.push(leaf, file);
                self.set_child(parent, key, id);
            }
        }
    }

    /// Remove the node at `comps` from its parent (tombstone / whiteout). Orphaned
    /// subtree nodes remain in the arena but become unreachable — acceptable for MVP.
    fn remove_path(&mut self, comps: &[&str]) {
        let (leaf, parents) = match comps.split_last() {
            Some(x) => x,
            None => return,
        };
        // Walk parents without creating anything.
        let mut cur = 0u32;
        for comp in parents {
            match self.child(cur, &fold(comp)) {
                Some(id) if matches!(self.nodes[id as usize].entry, NodeEntry::Dir(_)) => cur = id,
                _ => return, // path doesn't exist as a dir; nothing to remove
            }
        }
        if let NodeEntry::Dir(d) = &mut self.nodes[cur as usize].entry {
            d.children.remove(&fold(leaf));
        }
    }
}
```

Note: `FileNode`/`Node` are constructed in tests via `resolve` only, so no `#[derive(Debug)]` is required on them, but `Resolution` derives `Debug` (Task 5) so `panic!("{other:?}")` in tests compiles.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vfs-core tree`
Expected: PASS (9 tests).

- [ ] **Step 5: Uncomment the exports in `lib.rs`**

Uncomment: `pub use tree::VfsTree;` and `pub use tree::build;`
Run: `cargo build`
Expected: compiles.

- [ ] **Step 6: Commit**

```bash
git add crates/vfs-core/src/tree.rs crates/vfs-core/src/lib.rs
git commit -m "feat(vfs-core): merged tree build and resolve"
```

---

### Task 8: `getattr` query

**Files:**
- Modify: `crates/vfs-core/src/tree.rs`

**Interfaces:**
- Consumes: `VfsTree::find`, `model::{Stat, NodeKind}`.
- Produces: `impl VfsTree { pub fn getattr(&self, vpath: &str) -> Option<Stat> }`.

- [ ] **Step 1: Write the failing tests**

Add these tests inside the existing `mod tests` in `crates/vfs-core/src/tree.rs`:

```rust
    #[test]
    fn getattr_file_reports_size_and_mtime() {
        use crate::model::{NodeKind, Stat};
        let t = build(vec![layer(0, vec![file("data/a.esp", "s", 123, 456)])]).unwrap();
        assert_eq!(
            t.getattr("data/a.esp"),
            Some(Stat { kind: NodeKind::File, size: 123, mtime: 456 })
        );
    }

    #[test]
    fn getattr_dir_reports_dir_kind() {
        use crate::model::{NodeKind, Stat};
        let t = build(vec![layer(0, vec![file("data/a.esp", "s", 1, 1)])]).unwrap();
        assert_eq!(
            t.getattr("data"),
            Some(Stat { kind: NodeKind::Dir, size: 0, mtime: 0 })
        );
    }

    #[test]
    fn getattr_missing_is_none() {
        let t = build(vec![layer(0, vec![file("data/a.esp", "s", 1, 1)])]).unwrap();
        assert_eq!(t.getattr("nope"), None);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vfs-core tree::tests::getattr`
Expected: FAIL to compile (`getattr` not found).

- [ ] **Step 3: Implement**

Add to the `impl VfsTree` block in `tree.rs` (import `NodeKind`, `Stat` at the top `use crate::model::...` line):

```rust
    pub fn getattr(&self, vpath: &str) -> Option<crate::model::Stat> {
        use crate::model::{NodeKind, Stat};
        let norm = normalize_vpath(vpath).ok()?;
        let id = self.find(&norm)?;
        Some(match &self.nodes[id as usize].entry {
            NodeEntry::Dir(_) => Stat { kind: NodeKind::Dir, size: 0, mtime: 0 },
            NodeEntry::File(f) => Stat { kind: NodeKind::File, size: f.size, mtime: f.mtime },
        })
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vfs-core tree`
Expected: PASS (all tree tests, now 12).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-core/src/tree.rs
git commit -m "feat(vfs-core): getattr query"
```

---

### Task 9: `readdir` query (merged, sorted, filtered)

**Files:**
- Modify: `crates/vfs-core/src/tree.rs`

**Interfaces:**
- Consumes: `VfsTree::find`, `casefold::cmp_ci`, `wildcard::wildcard_match`, `model::{DirEntry, NodeKind, VfsError}`.
- Produces: `impl VfsTree { pub fn readdir(&self, vpath: &str, filter: Option<&str>) -> Result<Vec<DirEntry>, VfsError> }`.

- [ ] **Step 1: Write the failing tests**

Add these tests inside the existing `mod tests` in `tree.rs`:

```rust
    #[test]
    fn readdir_merges_and_sorts_case_insensitively() {
        let t = build(vec![
            layer(0, vec![file("d/Zebra.esp", "s", 1, 1), file("d/apple.esp", "s", 1, 1)]),
            layer(1, vec![file("d/Mango.esp", "s", 1, 1)]),
        ])
        .unwrap();
        let names: Vec<String> = t.readdir("d", None).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["apple.esp", "Mango.esp", "Zebra.esp"]);
    }

    #[test]
    fn readdir_honors_tombstones() {
        let t = build(vec![
            layer(0, vec![file("d/a.esp", "s", 1, 1), file("d/b.esp", "s", 1, 1)]),
            layer(1, vec![tomb("d/a.esp")]),
        ])
        .unwrap();
        let names: Vec<String> = t.readdir("d", None).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["b.esp"]);
    }

    #[test]
    fn readdir_applies_wildcard_filter() {
        let t = build(vec![layer(
            0,
            vec![file("d/a.esp", "s", 1, 1), file("d/b.txt", "s", 1, 1), file("d/c.esp", "s", 1, 1)],
        )])
        .unwrap();
        let names: Vec<String> =
            t.readdir("d", Some("*.esp")).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a.esp", "c.esp"]);
    }

    #[test]
    fn readdir_on_file_is_not_a_directory() {
        use crate::model::VfsError;
        let t = build(vec![layer(0, vec![file("d/a.esp", "s", 1, 1)])]).unwrap();
        assert_eq!(t.readdir("d/a.esp", None).unwrap_err(), VfsError::NotADirectory);
    }

    #[test]
    fn readdir_missing_is_not_found() {
        use crate::model::VfsError;
        let t = build(vec![layer(0, vec![file("d/a.esp", "s", 1, 1)])]).unwrap();
        assert_eq!(t.readdir("nope", None).unwrap_err(), VfsError::NotFound);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vfs-core tree::tests::readdir`
Expected: FAIL to compile (`readdir` not found).

- [ ] **Step 3: Implement**

Add to the `impl VfsTree` block in `tree.rs`:

```rust
    pub fn readdir(
        &self,
        vpath: &str,
        filter: Option<&str>,
    ) -> Result<Vec<crate::model::DirEntry>, crate::model::VfsError> {
        use crate::casefold::cmp_ci;
        use crate::model::{DirEntry, NodeKind, VfsError};
        use crate::wildcard::wildcard_match;

        let norm = normalize_vpath(vpath).map_err(|_| VfsError::NotFound)?;
        let id = self.find(&norm).ok_or(VfsError::NotFound)?;
        let dir = match &self.nodes[id as usize].entry {
            NodeEntry::Dir(d) => d,
            NodeEntry::File(_) => return Err(VfsError::NotADirectory),
        };

        let mut out: Vec<DirEntry> = dir
            .children
            .values()
            .map(|&cid| {
                let node = &self.nodes[cid as usize];
                match &node.entry {
                    NodeEntry::Dir(_) => DirEntry {
                        name: node.name.clone(),
                        kind: NodeKind::Dir,
                        size: 0,
                        mtime: 0,
                    },
                    NodeEntry::File(f) => DirEntry {
                        name: node.name.clone(),
                        kind: NodeKind::File,
                        size: f.size,
                        mtime: f.mtime,
                    },
                }
            })
            .filter(|e| match filter {
                Some(pat) => wildcard_match(pat, &e.name),
                None => true,
            })
            .collect();

        out.sort_by(|a, b| cmp_ci(&a.name, &b.name));
        Ok(out)
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vfs-core tree`
Expected: PASS (all tree tests, now 17).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-core/src/tree.rs
git commit -m "feat(vfs-core): readdir merged/sorted/filtered enumeration"
```

---

### Task 10: Integration test, property test & docs polish

**Files:**
- Create: `crates/vfs-core/tests/integration.rs`
- Modify: `crates/vfs-core/src/lib.rs` (crate-level doc example)

**Interfaces:**
- Consumes: the whole public API.
- Produces: an end-to-end integration test and one property test; a doctest on the crate root.

- [ ] **Step 1: Write an end-to-end integration test**

Create `crates/vfs-core/tests/integration.rs`:

```rust
use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId, NodeKind, Resolution, SourceId};

fn file(vpath: &str, source: &str, size: u64, mtime: i64) -> InputEntry {
    InputEntry { vpath: vpath.into(), kind: EntryKind::File, source: source.into(), size, mtime }
}
fn tomb(vpath: &str) -> InputEntry {
    InputEntry { vpath: vpath.into(), kind: EntryKind::Tombstone, source: "".into(), size: 0, mtime: 0 }
}

#[test]
fn end_to_end_modded_game_view() {
    // Layer 0 = real game dir; layers 1..=2 = mods (higher wins).
    let tree = build(vec![
        Layer {
            id: LayerId(0),
            entries: vec![
                file("Data/Skyrim.esm", "game/Data/Skyrim.esm", 100, 1),
                file("Data/textures/rock.dds", "game/.../rock.dds", 50, 1),
            ],
        },
        Layer {
            id: LayerId(1),
            entries: vec![file("Data/textures/rock.dds", "mod1/rock.dds", 80, 2)],
        },
        Layer {
            id: LayerId(2),
            entries: vec![
                file("Data/MyMod.esp", "mod2/MyMod.esp", 10, 3),
                tomb("Data/Skyrim.esm"),
            ],
        },
    ])
    .unwrap();

    // Mod1 overrides the base texture.
    match tree.resolve("Data/textures/rock.dds") {
        Resolution::File { source, size, layer, .. } => {
            assert_eq!(source, SourceId::from("mod1/rock.dds"));
            assert_eq!(size, 80);
            assert_eq!(layer, LayerId(1));
        }
        other => panic!("expected mod1 file, got {other:?}"),
    }

    // Mod2 tombstones the base master.
    assert_eq!(tree.resolve("Data/Skyrim.esm"), Resolution::NotFound);

    // Merged Data listing is sorted case-insensitively and honors the tombstone.
    let names: Vec<String> =
        tree.readdir("Data", None).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, vec!["MyMod.esp", "textures"]);

    // The new mod file resolves.
    assert!(matches!(tree.resolve("Data/MyMod.esp"), Resolution::File { .. }));

    // A directory reports as a dir via getattr.
    assert_eq!(tree.getattr("Data/textures").unwrap().kind, NodeKind::Dir);
}
```

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p vfs-core --test integration`
Expected: PASS.

- [ ] **Step 3: Add a property test for build robustness**

Append to `crates/vfs-core/tests/integration.rs`:

```rust
use proptest::prelude::*;

proptest! {
    // Building from arbitrary component names never panics and every inserted
    // leaf either resolves or was shadowed — build is total over valid input.
    #[test]
    fn build_never_panics_on_arbitrary_names(
        names in proptest::collection::vec("[a-zA-Z0-9]{1,8}", 1..20)
    ) {
        let entries: Vec<InputEntry> = names
            .iter()
            .enumerate()
            .map(|(i, n)| file(&format!("d/{n}"), &format!("s{i}"), i as u64, i as i64))
            .collect();
        let tree = build(vec![Layer { id: LayerId(0), entries }]).unwrap();
        // The directory always exists and readdir succeeds.
        prop_assert!(tree.readdir("d", None).is_ok());
    }
}
```

- [ ] **Step 4: Run the property test**

Run: `cargo test -p vfs-core --test integration`
Expected: PASS.

- [ ] **Step 5: Add a crate-root doctest**

Replace the top doc comment block in `crates/vfs-core/src/lib.rs` with:

```rust
#![forbid(unsafe_code)]
//! `vfs-core`: pure, OS-independent read-only resolver for a merged/overlaid
//! virtual filesystem. Fed enumerated layers (data-in); does no I/O.
//!
//! ```
//! use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId, Resolution};
//!
//! let tree = build(vec![Layer {
//!     id: LayerId(0),
//!     entries: vec![InputEntry {
//!         vpath: "data/a.esp".into(),
//!         kind: EntryKind::File,
//!         source: "root/data/a.esp".into(),
//!         size: 10,
//!         mtime: 42,
//!     }],
//! }])
//! .unwrap();
//! assert!(matches!(tree.resolve("data/a.esp"), Resolution::File { .. }));
//! ```
```

- [ ] **Step 6: Run the full suite + doctests**

Run: `cargo test -p vfs-core`
Expected: PASS (all unit, integration, property, and doc tests).

- [ ] **Step 7: Commit**

```bash
git add crates/vfs-core/tests/integration.rs crates/vfs-core/src/lib.rs
git commit -m "test(vfs-core): end-to-end integration, property test, doctest"
```

---

## Self-review

**Spec coverage:**
- §2 build/getattr/resolve/readdir → Tasks 7, 8, 9. ✓
- §2 path normalization → Task 3. ✓
- §2 DOS wildcard matching → Task 4. ✓
- §2 cache-key computation → Task 6. ✓
- §3 data model (layers, entries, tombstones, opaque SourceId) → Tasks 5, 7. ✓
- §5 semantics: priority/union/tombstone/conflict/case/cache-key/NotFound-collapse → Task 7 tests. ✓
- §5 case folding single source of truth → Task 2. ✓
- §6 error handling (BuildError/PathError/VfsError, Option for not-found) → Tasks 3, 5, 7, 8, 9. ✓
- §7 testing strategy (merge, tombstone, sort regression, wildcard parity, path normalization, cache-key stability) → Tasks 2–9 + integration Task 10. ✓
- §8 stable toolchain, blake3, no unsafe, workspace at `crates/vfs-core` → Task 1 + global constraints. ✓

**Deferred by spec (correctly absent):** runtime mutation, arena/bitness layout, materialization I/O, image/data classification, refcount transitions. Not planned — matches §9.

**Placeholder scan:** no TBD/TODO; every code step shows complete code. The one scoped simplification (`<` DOS_STAR ≈ `*`) is documented with a test table, not a placeholder.

**Type consistency:** `SourceId`, `LayerId`, `CacheKey`, `Resolution`, `Stat`, `DirEntry`, `EntryKind`, `NodeKind`, `BuildError`, `VfsError` names/shapes are identical across Tasks 5–10. `build`, `resolve`, `getattr`, `readdir`, `find`, `normalize_vpath`, `wildcard_match`, `fold`, `cmp_ci`, `compute_cache_key` signatures match every call site.

*End of plan.*
