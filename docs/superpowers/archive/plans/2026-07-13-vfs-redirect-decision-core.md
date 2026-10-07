# vfs-redirect Decision Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A pure `vfs-redirect` crate whose `RootMap::decide` maps an incoming NT open path + a published snapshot to either `PassThrough` or `Redirect { target_nt }` (the mod backing file's NT path) — the injected shim's decision brain, testable with no OS APIs.

**Architecture:** `decide` normalizes the NT path with `vfs_core::normalize_vpath` (which strips `\??\`/`\\?\` and unifies separators, yielding a `/`-joined form including the drive), matches it component-wise and case-insensitively against the stored root, folds the remainder, calls `SnapshotReader::resolve`, and renders a `File` result's `source` back into an NT path. All logic is `#![forbid(unsafe_code)]` and works on `&str`; thin UTF-16 helpers bridge `UNICODE_STRING` for the future hook layer.

**Tech Stack:** Rust (stable). Deps: `vfs-core` (path: `normalize_vpath`, `fold`, `PathError`), `vfs-shared` (path: `SnapshotReader`, `SnapResolution`; `bridge` feature enabled for tests to build fixtures).

## Global Constraints

- Stable Rust; crate attribute `#![forbid(unsafe_code)]`.
- No panics; `decide` is total — any malformed/out-of-root/unresolvable input → `PassThrough` (fail safe: never redirect what you can't positively resolve).
- `Decision` derives `Debug, Clone, PartialEq, Eq` (used in `assert_eq!`).
- Backing-source contract: the director stores `source` as a UTF-8 absolute Win32 path with a drive letter and no NT prefix (e.g. `D:\Mods\Cool\foo.esp`); `render_nt` prepends `\??\`, but passes through a `source` already beginning with `\??\`/`\\?\` unchanged.
- `SnapshotReader::resolve(folded: &[&str])` requires **pre-folded** components — always `fold` each remainder component before calling it.
- `string_to_utf16` produces NO trailing NUL (`UNICODE_STRING` is length-counted).

---

### Task 1: Crate scaffold, workspace wiring, and `vfs_core::fold` export

**Files:**
- Create: `crates/vfs-redirect/Cargo.toml`
- Create: `crates/vfs-redirect/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`)
- Modify: `crates/vfs-core/src/lib.rs` (export `fold`)

**Interfaces:**
- Consumes: nothing yet.
- Produces: an empty `vfs-redirect` crate that builds; `vfs_core::fold` becomes public.

- [ ] **Step 1: Export `fold` from `vfs-core`**

In `crates/vfs-core/src/lib.rs`, add to the `pub use path::...` area:

```rust
pub use casefold::fold;
```

(Place it next to `pub use path::{normalize_vpath, PathError};`.)

- [ ] **Step 2: Verify `vfs-core` still builds and `fold` is reachable**

Run: `cargo build -p vfs-core`
Expected: compiles (no warnings — `fold` was already used internally, now also re-exported).

- [ ] **Step 3: Add the crate to workspace members**

In root `Cargo.toml`, add `"crates/vfs-redirect"` to the `members` array (sorted after `crates/vfs-ipc`, before `crates/vfs-server` — keep alphabetical).

- [ ] **Step 4: Write `crates/vfs-redirect/Cargo.toml`**

```toml
[package]
name = "vfs-redirect"
version = "0.1.0"
edition = "2021"

[dependencies]
vfs-core = { path = "../vfs-core" }
vfs-shared = { path = "../vfs-shared" }

[dev-dependencies]
# `bridge` (test-only) provides `flatten` for building snapshot fixtures.
vfs-shared = { path = "../vfs-shared", features = ["bridge"] }
```

- [ ] **Step 5: Write a minimal `crates/vfs-redirect/src/lib.rs`**

```rust
#![forbid(unsafe_code)]

//! The injected shim's pure redirect-decision core: map an incoming NT open path
//! + a published snapshot to pass-through vs redirect-to-backing-file.

/// The outcome of inspecting one NT open path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Let the original NT open proceed unchanged.
    PassThrough,
    /// Reissue the open against this NT path (the mod backing file).
    Redirect { target_nt: String },
}
```

- [ ] **Step 6: Build to verify the workspace resolves**

Run: `cargo build -p vfs-redirect`
Expected: compiles (a dead-code warning on `Decision`/`Redirect` is acceptable at this step).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/vfs-core/src/lib.rs crates/vfs-redirect/Cargo.toml crates/vfs-redirect/src/lib.rs
git commit -m "vfs-redirect: crate scaffold, workspace wiring, export vfs_core::fold"
```

---

### Task 2: `RootMap::new` + UTF-16 conversion helpers

**Files:**
- Modify: `crates/vfs-redirect/src/lib.rs`
- Test: `crates/vfs-redirect/src/lib.rs` (`#[cfg(test)]` module)

**Interfaces:**
- Consumes: `vfs_core::{normalize_vpath, PathError}`.
- Produces:
  - `RootMap::new(root: &str) -> Result<RootMap, vfs_core::PathError>`
  - `utf16_to_string(units: &[u16]) -> String`
  - `string_to_utf16(s: &str) -> Vec<u16>`
  - `RootMap` stores its normalized root components (original case) in a private `Vec<String>` field named `root`, consumed by Task 3's `decide`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/vfs-redirect/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_normalizes_nt_and_win32_roots() {
        // Both forms normalize to the same component vector.
        let nt = RootMap::new(r"\??\C:\Games\Skyrim").unwrap();
        let win32 = RootMap::new(r"C:\Games\Skyrim").unwrap();
        assert_eq!(nt.root_components(), win32.root_components());
        assert_eq!(nt.root_components(), vec!["C:", "Games", "Skyrim"]);
    }

    #[test]
    fn utf16_round_trips() {
        let s = "C:\\Games\\Skyrim\\Data\\foo.esp";
        assert_eq!(utf16_to_string(&string_to_utf16(s)), s);
        // No trailing NUL is appended.
        assert_eq!(*string_to_utf16("ab").last().unwrap(), b'b' as u16);
    }

    #[test]
    fn utf16_lossy_does_not_panic_on_unpaired_surrogate() {
        let units: [u16; 2] = [0xD800, b'x' as u16]; // lone high surrogate
        let _ = utf16_to_string(&units); // must not panic
    }
}
```

Note: the test calls `nt.root_components()` — a test-visible accessor. Add it as a
small public method returning `&[String]` (harmless, and useful for debugging).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-redirect`
Expected: FAIL to compile (`RootMap`, `utf16_to_string`, `string_to_utf16`, `root_components` undefined).

- [ ] **Step 3: Implement `RootMap::new`, `root_components`, and the helpers**

Add to `crates/vfs-redirect/src/lib.rs` (above the test module):

```rust
use vfs_core::{normalize_vpath, PathError};

/// The managed VFS install root (mount point), as normalized path components.
pub struct RootMap {
    /// Normalized root components in original case, e.g. `["C:", "Games", "Skyrim"]`.
    root: Vec<String>,
}

impl RootMap {
    /// `root` may be NT (`\??\C:\Games\Skyrim`) or Win32 (`C:\Games\Skyrim`).
    pub fn new(root: &str) -> Result<Self, PathError> {
        let norm = normalize_vpath(root)?;
        let root = if norm.is_empty() {
            Vec::new()
        } else {
            norm.split('/').map(str::to_string).collect()
        };
        Ok(RootMap { root })
    }

    /// The normalized root components (original case). For tests/diagnostics.
    pub fn root_components(&self) -> &[String] {
        &self.root
    }
}

/// Decode a length-counted UTF-16 buffer (a `UNICODE_STRING` body) to a `String`.
/// Lossy: unpaired surrogates become U+FFFD rather than panicking.
pub fn utf16_to_string(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// Encode a `&str` as UTF-16 with NO trailing NUL (`UNICODE_STRING` is counted).
pub fn string_to_utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}
```

- [ ] **Step 4: Run to verify passing**

Run: `cargo test -p vfs-redirect`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-redirect/src/lib.rs
git commit -m "vfs-redirect: RootMap::new and UTF-16 conversion helpers"
```

---

### Task 3: `RootMap::decide` + `render_nt`

**Files:**
- Modify: `crates/vfs-redirect/src/lib.rs`
- Test: `crates/vfs-redirect/src/lib.rs` (`#[cfg(test)]` module)

**Interfaces:**
- Consumes: `RootMap.root` (Task 2), `vfs_core::fold`, `vfs_shared::{SnapshotReader, SnapResolution}`, `Decision` (Task 1).
- Produces: `RootMap::decide(&self, nt_path: &str, snap: &vfs_shared::SnapshotReader) -> Decision`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block. This includes a fixture builder using
the `bridge` dev-feature:

```rust
    use vfs_shared::SnapshotReader;

    // Build a snapshot with two virtual files under `data/`.
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
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    }

    fn root() -> RootMap {
        RootMap::new(r"\??\C:\Games\Skyrim").unwrap()
    }

    #[test]
    fn redirects_a_virtual_file() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\Data\foo.esp", &snap);
        assert_eq!(
            d,
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
    }

    #[test]
    fn redirect_is_case_insensitive_on_root_and_remainder() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\c:\games\SKYRIM\DATA\Foo.ESP", &snap);
        assert_eq!(
            d,
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
    }

    #[test]
    fn passes_through_outside_root() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Windows\System32\kernel32.dll", &snap);
        assert_eq!(d, Decision::PassThrough);
    }

    #[test]
    fn passes_through_under_root_but_not_virtualized() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\Data\notmod.esp", &snap);
        assert_eq!(d, Decision::PassThrough);
    }

    #[test]
    fn passes_through_a_virtual_directory() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\Data", &snap);
        assert_eq!(d, Decision::PassThrough);
    }

    #[test]
    fn passes_through_escaping_path_without_panic() {
        // Four `..` pop past the drive component, so normalize_vpath returns
        // PathError::EscapesRoot; decide must fail safe to PassThrough, not panic.
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\..\..\..\..\evil", &snap);
        assert_eq!(d, Decision::PassThrough);
    }

    #[test]
    fn win32_form_root_matches_nt_form_open() {
        let bytes = snapshot_bytes();
        let snap = SnapshotReader::open(&bytes).unwrap();
        let win32_root = RootMap::new(r"C:\Games\Skyrim").unwrap();
        let d = win32_root.decide(r"\??\C:\Games\Skyrim\Data\foo.esp", &snap);
        assert_eq!(
            d,
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
    }

    #[test]
    fn source_already_nt_prefixed_is_not_double_prefixed() {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/foo.esp".into(),
                kind: EntryKind::File,
                source: r"\??\D:\Mods\Cool\foo.esp".into(),
                size: 10,
                mtime: 1,
            }],
        }])
        .unwrap();
        let bytes = vfs_shared::bridge::flatten(&tree);
        let snap = SnapshotReader::open(&bytes).unwrap();
        let d = root().decide(r"\??\C:\Games\Skyrim\Data\foo.esp", &snap);
        assert_eq!(
            d,
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-redirect`
Expected: FAIL to compile (`decide` not defined).

- [ ] **Step 3: Implement `decide` and `render_nt`**

Add `use vfs_core::fold;` and `use vfs_shared::{SnapResolution, SnapshotReader};`
to the imports at the top of `crates/vfs-redirect/src/lib.rs`, then add to
`impl RootMap`:

```rust
    /// Decide how to handle an incoming NT open path.
    ///
    /// Fail-safe: any path that is malformed, outside the root, or does not
    /// positively resolve to a virtualized file yields `PassThrough`.
    pub fn decide(&self, nt_path: &str, snap: &SnapshotReader) -> Decision {
        let norm = match normalize_vpath(nt_path) {
            Ok(n) => n,
            Err(_) => return Decision::PassThrough,
        };
        let comps: Vec<&str> =
            if norm.is_empty() { Vec::new() } else { norm.split('/').collect() };
        if comps.len() < self.root.len() {
            return Decision::PassThrough;
        }
        // Component-wise, case-insensitive root prefix match.
        for (r, c) in self.root.iter().zip(comps.iter()) {
            if fold(r) != fold(c) {
                return Decision::PassThrough;
            }
        }
        // Fold the remainder and resolve it against the snapshot.
        let folded: Vec<String> = comps[self.root.len()..].iter().map(|c| fold(c)).collect();
        let folded_refs: Vec<&str> = folded.iter().map(String::as_str).collect();
        match snap.resolve(&folded_refs) {
            SnapResolution::File { source, .. } => {
                Decision::Redirect { target_nt: render_nt(&source) }
            }
            SnapResolution::Dir | SnapResolution::NotFound => Decision::PassThrough,
        }
    }
```

And add this free function (below `impl RootMap`):

```rust
/// Render a backing `source` (a UTF-8 absolute Win32 path, per the director's
/// contract) as an NT DOS-device path. A `source` already carrying an NT/DOS
/// long-path prefix is returned unchanged rather than double-prefixed.
fn render_nt(source: &[u8]) -> String {
    let s = String::from_utf8_lossy(source);
    if s.starts_with(r"\??\") || s.starts_with(r"\\?\") {
        s.into_owned()
    } else {
        format!(r"\??\{s}")
    }
}
```

- [ ] **Step 4: Run to verify passing**

Run: `cargo test -p vfs-redirect`
Expected: PASS (all Task 2 + Task 3 tests).

If `SnapshotReader::open` / `resolve` / `SnapResolution` names differ from what
this task assumes, STOP and report — check `crates/vfs-shared/src/reader.rs`
(confirmed: `open(bytes: &[u8]) -> Result<Self, LayoutError>`, `resolve(folded:
&[&str]) -> SnapResolution`, `SnapResolution::File { source: Vec<u8>, .. }`).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-redirect/src/lib.rs
git commit -m "vfs-redirect: RootMap::decide with NT-path redirect resolution"
```

---

### Task 4: Verification sweep

**Files:** none (verification only).

- [ ] **Step 1: Full workspace test**

Run: `cargo test --workspace`
Expected: all crates green, including the new `vfs-redirect` tests.

- [ ] **Step 2: Warning check**

Run: `cargo build --workspace 2>&1 | rg -i "warning" || echo "no warnings"`
Expected: `no warnings`.

- [ ] **Step 3: Unsafe check**

Run: confirm `crates/vfs-redirect/src/lib.rs` begins with `#![forbid(unsafe_code)]` and contains no `unsafe`.
Expected: no `unsafe` anywhere in the crate.

- [ ] **Step 4: Commit Cargo.lock (if changed)**

```bash
git add Cargo.lock
git commit -m "vfs-redirect: update Cargo.lock" || echo "nothing to commit"
```

---

## Self-Review Notes

- **Spec coverage:** `RootMap` + `new` (Task 2), `Decision` (Task 1), `decide` algorithm steps 1–3 (Task 3), `render_nt` incl. already-prefixed guard (Task 3), UTF-16 helpers (Task 2), `vfs_core::fold` export (Task 1), workspace wiring (Task 1). Every §6 test case is present: redirect hit, case-insensitive, outside-root, under-root-not-found, dir, escaping, win32-root, source-already-prefixed, utf16 round-trip + lossy.
- **Derives:** `Decision` derives `Debug, Clone, PartialEq, Eq` (Task 1) — required by every `assert_eq!`. `RootMap` is never compared/unwrapped-err in tests, so no derive needed on it.
- **Type consistency:** `decide` returns `Decision`; `render_nt` takes `&[u8]` matching `SnapResolution::File.source: Vec<u8>`; `resolve` receives `&[&str]` of folded components. `root_components() -> &[String]` matches the `root: Vec<String>` field.
- **Feature nuance:** `vfs-shared` appears in both `[dependencies]` (default) and `[dev-dependencies]` (with `bridge`); cargo unifies to bridge-enabled for test builds only — the standard pattern, and the crate's own lib never touches `bridge`.
