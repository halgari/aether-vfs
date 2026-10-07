# Read-Path Decision Transforms Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `vfs-redirect`'s pure transforms tombstone-aware: `decide` returns a real `Deny` for a tombstoned path, and a new `query_attributes` answers path-based attribute queries — both routed through one shared `locate` helper, exhaustively unit-tested.

**Architecture:** Extract the normalize + case-insensitive root-match + `snap.resolve` logic into a private `locate(nt_path, snap) -> Located` helper. `decide` and `query_attributes` both consume `Located` and map each `SnapResolution` per the behavior matrix. Pure, `#![forbid(unsafe_code)]`.

**Tech Stack:** Rust (stable). `vfs-redirect`; dev-deps `vfs-core` + `vfs-shared` (`bridge`) for fixtures.

## Global Constraints

- Stable; `vfs-redirect` stays `#![forbid(unsafe_code)]`.
- `Decision` and `AttrDecision` derive `Debug, Clone, PartialEq, Eq` (asserted via `assert_eq!`).
- Transforms are total & fail-safe: malformed/out-of-root/`NotFound` → `PassThrough`; only a `Tombstone` → `Deny`; only a `File` → `Redirect`/`Attributes{is_dir:false}`; only a `Dir` → `Attributes{is_dir:true}`.
- Adding `Decision::Deny` is compile-safe downstream: `vfs-shim`'s hook uses `if let Decision::Redirect { .. }` (non-exhaustive), so `Deny` currently falls through to pass-through in the hook until Slice D wires it to `STATUS_OBJECT_NAME_NOT_FOUND`. Do not change `vfs-shim` in this slice.

---

### Task 1: `locate` helper + `Decision::Deny` (tombstone → Deny)

**Files:**
- Modify: `crates/vfs-redirect/src/lib.rs`
- Test: `crates/vfs-redirect/src/lib.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `Decision::Deny`; a private `Located` enum + `RootMap::locate`; `decide` maps `Tombstone → Deny`.

- [ ] **Step 1: Add a tombstone to the shared test fixture and write the failing Deny test**

In the `#[cfg(test)] mod tests` block of `crates/vfs-redirect/src/lib.rs`, extend
`snapshot_bytes` to include a tombstone (add `InputEntry`/`EntryKind` to its `use`):

```rust
    fn snapshot_bytes() -> Vec<u8> {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let file = |vpath: &str, source: &str| InputEntry {
            vpath: vpath.into(),
            kind: EntryKind::File,
            source: source.into(),
            size: 10,
            mtime: 1,
        };
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![
                file("data/foo.esp", r"D:\Mods\Cool\foo.esp"),
                file("data/sub/bar.dds", r"D:\Mods\Cool\bar.dds"),
                InputEntry {
                    vpath: "data/deleted.esp".into(),
                    kind: EntryKind::Tombstone,
                    source: "".into(),
                    size: 0,
                    mtime: 0,
                },
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    }
```

Add the Deny test:

```rust
    #[test]
    fn decide_denies_a_tombstoned_path() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().decide(r"\??\C:\Games\Skyrim\Data\deleted.esp", &snap),
            Decision::Deny
        );
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-redirect decide_denies_a_tombstoned_path`
Expected: FAIL to compile (`Decision::Deny` does not exist).

- [ ] **Step 3: Add `Decision::Deny`, the `Located` helper, and rewrite `decide`**

In `crates/vfs-redirect/src/lib.rs`:

(a) Add the variant to `Decision`:

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Let the original NT open proceed unchanged.
    PassThrough,
    /// Reissue the open against this NT path (the mod backing file).
    Redirect { target_nt: String },
    /// The path is tombstoned (mod-deleted); the hook must return
    /// STATUS_OBJECT_NAME_NOT_FOUND rather than open or pass through.
    Deny,
}
```

(b) Add the private helper + enum (place `Located` near `RootMap`, the `impl` method inside `impl RootMap`):

```rust
/// Where an NT path lands relative to the managed root.
enum Located {
    /// Not under the root, or malformed/escaping — never virtualized.
    Outside,
    /// Under the root; here is the snapshot's answer for the remainder.
    Resolved(SnapResolution),
}

impl RootMap {
    /// Normalize + case-insensitively match the root + resolve the remainder.
    fn locate(&self, nt_path: &str, snap: &SnapshotReader) -> Located {
        let norm = match normalize_vpath(nt_path) {
            Ok(n) => n,
            Err(_) => return Located::Outside,
        };
        let comps: Vec<&str> =
            if norm.is_empty() { Vec::new() } else { norm.split('/').collect() };
        if comps.len() < self.root.len() {
            return Located::Outside;
        }
        for (r, c) in self.root.iter().zip(comps.iter()) {
            if fold(r) != fold(c) {
                return Located::Outside;
            }
        }
        let folded: Vec<String> = comps[self.root.len()..].iter().map(|c| fold(c)).collect();
        let folded_refs: Vec<&str> = folded.iter().map(String::as_str).collect();
        Located::Resolved(snap.resolve(&folded_refs))
    }
}
```

(c) Replace the body of `decide` to use `locate`:

```rust
    pub fn decide(&self, nt_path: &str, snap: &SnapshotReader) -> Decision {
        match self.locate(nt_path, snap) {
            Located::Resolved(SnapResolution::File { source, .. }) => {
                Decision::Redirect { target_nt: render_nt(&source) }
            }
            Located::Resolved(SnapResolution::Tombstone) => Decision::Deny,
            Located::Resolved(SnapResolution::Dir)
            | Located::Resolved(SnapResolution::NotFound)
            | Located::Outside => Decision::PassThrough,
        }
    }
```

- [ ] **Step 4: Run to verify passing**

Run: `cargo test -p vfs-redirect`
Expected: PASS — the new Deny test plus all pre-existing `decide` tests (the
fixture's added tombstone doesn't affect `foo.esp`/`sub`/outside assertions).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-redirect/src/lib.rs
git commit -m "vfs-redirect: Decision::Deny for tombstones + shared locate() helper"
```

---

### Task 2: `query_attributes` + `AttrDecision`

**Files:**
- Modify: `crates/vfs-redirect/src/lib.rs`
- Test: `crates/vfs-redirect/src/lib.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `RootMap::locate` (Task 1).
- Produces: `AttrDecision`, `RootMap::query_attributes(nt_path, snap) -> AttrDecision`.

- [ ] **Step 1: Write the failing matrix tests**

Add to the tests block:

```rust
    #[test]
    fn attrs_of_a_virtual_file() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().query_attributes(r"\??\C:\Games\Skyrim\Data\foo.esp", &snap),
            AttrDecision::Attributes { is_dir: false, size: 10, mtime: 1 }
        );
    }

    #[test]
    fn attrs_of_a_virtual_directory() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().query_attributes(r"\??\C:\Games\Skyrim\Data", &snap),
            AttrDecision::Attributes { is_dir: true, size: 0, mtime: 0 }
        );
    }

    #[test]
    fn attrs_of_a_tombstone_deny() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().query_attributes(r"\??\C:\Games\Skyrim\Data\deleted.esp", &snap),
            AttrDecision::Deny
        );
    }

    #[test]
    fn attrs_under_root_not_virtualized_passes_through() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().query_attributes(r"\??\C:\Games\Skyrim\Data\real.esp", &snap),
            AttrDecision::PassThrough
        );
    }

    #[test]
    fn attrs_outside_root_passes_through() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().query_attributes(r"\??\C:\Windows\notepad.exe", &snap),
            AttrDecision::PassThrough
        );
    }

    #[test]
    fn attrs_are_case_insensitive() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        assert_eq!(
            root().query_attributes(r"\??\c:\games\SKYRIM\DATA\Foo.ESP", &snap),
            AttrDecision::Attributes { is_dir: false, size: 10, mtime: 1 }
        );
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-redirect query_attributes`
Expected: FAIL to compile (`AttrDecision`/`query_attributes` undefined).

- [ ] **Step 3: Implement `AttrDecision` + `query_attributes`**

Add the enum (near `Decision`):

```rust
/// The outcome of a path-based attribute query (NtQueryAttributesFile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrDecision {
    /// Let the original query proceed unchanged.
    PassThrough,
    /// Answer from the snapshot with these attributes.
    Attributes { is_dir: bool, size: u64, mtime: i64 },
    /// Tombstoned: return not-found rather than reveal a hidden real file.
    Deny,
}
```

Add the method to `impl RootMap`:

```rust
    /// Answer a path-based attribute query against the snapshot.
    pub fn query_attributes(&self, nt_path: &str, snap: &SnapshotReader) -> AttrDecision {
        match self.locate(nt_path, snap) {
            Located::Resolved(SnapResolution::File { size, mtime, .. }) => {
                AttrDecision::Attributes { is_dir: false, size, mtime }
            }
            Located::Resolved(SnapResolution::Dir) => {
                AttrDecision::Attributes { is_dir: true, size: 0, mtime: 0 }
            }
            Located::Resolved(SnapResolution::Tombstone) => AttrDecision::Deny,
            Located::Resolved(SnapResolution::NotFound) | Located::Outside => {
                AttrDecision::PassThrough
            }
        }
    }
```

- [ ] **Step 4: Run to verify passing**

Run: `cargo test -p vfs-redirect`
Expected: PASS (all `decide` + all 6 `query_attributes` matrix tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-redirect/src/lib.rs
git commit -m "vfs-redirect: query_attributes transform + AttrDecision"
```

---

### Task 3: Verification sweep

**Files:** none.

- [ ] **Step 1: Full workspace test**

Run: `cargo test --workspace`
Expected: all green. Adding `Decision::Deny` must NOT break `vfs-shim` (its hook
uses `if let Decision::Redirect`). If some other site has an exhaustive match on
`Decision` and fails to compile, add a `Decision::Deny => { /* pass through for
now; Slice D honors it */ }` arm and note it — but no such site is expected.

- [ ] **Step 2: Warning check**

Run: `cargo build --workspace 2>&1 | rg -i "warning" || echo "no warnings"`
Expected: `no warnings`.

- [ ] **Step 3: Commit Cargo.lock (if changed)**

```bash
git add Cargo.lock
git commit -m "read-path transforms: update Cargo.lock" || echo "nothing to commit"
```

---

## Self-Review Notes

- **Spec coverage:** `Decision::Deny` + tombstone mapping (Task 1), `locate` DRY helper (Task 1), `query_attributes` + `AttrDecision` full matrix (Task 2), downstream compile check (Task 3). Every §6 matrix row is a test.
- **Derives:** `Decision` already derives `Debug, Clone, PartialEq, Eq`; `AttrDecision` gets the same — both used in `assert_eq!`.
- **DRY:** `decide` and `query_attributes` share `locate`; the old inline normalize/match logic in `decide` is fully replaced (no duplicated root-matching).
- **Downstream safety:** `vfs-shim` unchanged; its `if let Decision::Redirect` ignores `Deny` (interim pass-through) until Slice D. The existing `vfs-shim` integration test (redirect of a live open) is unaffected — it exercises a `File`, not a tombstone.
