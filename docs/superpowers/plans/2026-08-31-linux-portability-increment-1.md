# Linux Portability Increment 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `vfs-director` — the userspace FUSE kernel — compiles and runs its unit tests on Linux, by severing its three Windows dependency edges without changing any behaviour or public interface on Windows.

**Architecture:** Three dependency edges are cut, each by moving code to a portable home and re-exporting from the original location so no call site changes. A new `vfs-pe` crate takes the pure PE byte-parsing core out of the Windows-only `vfs-inject`. `overlay_layer_dir` moves to `vfs-provider`, which already defines the `RootId` it takes. The shared-memory ring modules go behind `cfg(windows)`, since FUSE — not the ring — is the Linux transport.

**Tech Stack:** Rust 2021, cargo workspace at `rust/`. Verification uses `cargo check --target x86_64-unknown-linux-gnu` (no cross-linker needed, because `check` does not link) and the existing Windows `cargo test` / `cargo clippy` gates.

**Spec:** `docs/superpowers/specs/2026-08-31-linux-fuse-proton-portability-design.md`

## Global Constraints

- **Windows behaviour must not change.** This is a dependency refactor. Every moved symbol keeps its original import path via a re-export.
- **No test may be edited to make the suite green.** (Spec, Definition of done #3.) Adding new tests is expected; relaxing an existing assertion is a plan failure — stop and report instead.
- **`vfs-embed` and `vfs-node` public surfaces stay byte-identical to `f0a55ef`.** Neither crate is modified by this plan.
- **`vfs-provider` must keep zero dependencies.** Its `[dependencies]` section is empty today; `overlay_layer_dir` needs only `std`. Do not add a dependency to it.
- **Do not touch the wire protocol.** `bin/regen-protocol` must produce no diff under `resources/`.
- **Work stays on branch `worktree-linux-fuse-proton`.** Nothing is pushed to `master` during this plan.
- All `cargo` commands run from `rust/` unless stated otherwise.
- The Linux target must be installed once: `rustup target add x86_64-unknown-linux-gnu`.

---

## File Structure

**Created:**
- `rust/crates/vfs-pe/Cargo.toml` — new crate, zero dependencies
- `rust/crates/vfs-pe/src/lib.rs` — pure PE byte parsing: header reads, image flattening, import-table names, system-DLL classification

**Modified:**
- `rust/Cargo.toml` — add `crates/vfs-pe` to workspace members
- `rust/crates/vfs-inject/src/map.rs` — delete the pure parsing functions, re-export from `vfs-pe`
- `rust/crates/vfs-inject/src/pe.rs` — delete `is_system_import_dll` / `pe_looks_like_image`, re-export from `vfs-pe`
- `rust/crates/vfs-inject/src/lib.rs:27-30` — `import_dll_names_of_pe` delegates to `vfs-pe`
- `rust/crates/vfs-inject/Cargo.toml` — add `vfs-pe` dependency
- `rust/crates/vfs-provider/src/path.rs` — gains `overlay_layer_dir`
- `rust/crates/vfs-provider/src/lib.rs:16` — add `overlay_layer_dir` to the crate-root re-export. `lib.rs` re-exports *selected* items from `path.rs` (`pub use path::{RootId, VPath};`), so a function added to `path.rs` is not reachable as `vfs_provider::overlay_layer_dir` until it is named here — and that spelling is what Task 3 Step 6 requires.
- `rust/crates/vfs-shim/Cargo.toml` — add a `vfs-provider` dependency. `vfs-shim` currently reaches `RootId` transitively through `vfs-redirect`'s re-export and has no direct edge, so its own re-export of `overlay_layer_dir` will not compile without one. `vfs-provider` has no dependencies, so this adds no transitive weight.
- `rust/crates/vfs-shim/src/overlay.rs:12-29` — delete the definition, re-export from `vfs-provider`
- `rust/crates/vfs-director/src/stage.rs:358,382,392,425` — call `vfs_pe::` instead of `vfs_inject::`
- `rust/crates/vfs-director/src/lib.rs:25,29,42` — gate `ipc`, re-export `overlay_layer_dir` from `vfs-provider`. (Task 5 originally gated `ring_dispatch` too; the final review found it has no Windows dependency — only `ipc.rs` touches `vfs_win` — so it was ungated, and its five protocol-translation tests now run in Linux CI. See Task 5's callout and spec §3.)
- `rust/crates/vfs-director/Cargo.toml` — drop `vfs-win`/`vfs-inject`/`vfs-shim`; move `windows-sys` and dev-deps to `cfg(windows)` tables
- `rust/crates/vfs-director/tests/unicode_case_fold_across_the_ring.rs` — add `#![cfg(windows)]`
- `.github/workflows/ci.yml` — extend `rust-linux-portable`

**Why `vfs-pe` is one file:** the extracted surface is ~9 small functions of pure byte reading with no internal layering. Splitting it would be technical-layer decomposition, not responsibility decomposition.

---

### Task 1: The `vfs-pe` crate

**Files:**
- Create: `rust/crates/vfs-pe/Cargo.toml`
- Create: `rust/crates/vfs-pe/src/lib.rs`
- Modify: `rust/Cargo.toml` (workspace members)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn pe_looks_like_image(pe: &[u8]) -> bool`
  - `pub fn is_system_import_dll(name: &str) -> bool`
  - `pub fn is_pe32_plus(img: &[u8], e_lfanew: usize) -> bool`
  - `pub fn dd_base(img: &[u8], e_lfanew: usize) -> usize`
  - `pub fn build_image(raw: &[u8]) -> Result<(Vec<u8>, u64, usize), &'static str>`
  - `pub fn import_dll_names(img: &[u8], e_lfanew: usize) -> Vec<String>`
  - `pub fn apply_relocs(img: &mut [u8], e_lfanew: usize, image_base: u64, new_base: u64)`
  - `pub fn export_rva(img: &[u8], e_lfanew: usize, name: &[u8]) -> Result<u32, &'static str>`
  - `pub fn import_dll_names_of_pe(raw: &[u8]) -> Option<Vec<String>>`

- [ ] **Step 1: Add the crate to the workspace**

In `rust/Cargo.toml`, add to `members` immediately after `"crates/vfs-provider",`:

```toml
  # Pure PE byte parsing, extracted from vfs-inject so the director can stage
  # Windows executables on any host OS. No OS API and no dependencies: a PE is
  # a file format, not a platform. See
  # docs/superpowers/specs/2026-08-31-linux-fuse-proton-portability-design.md.
  "crates/vfs-pe",
```

- [ ] **Step 2: Create the manifest**

`rust/crates/vfs-pe/Cargo.toml`:

```toml
[package]
name = "vfs-pe"
version = "0.1.0"
edition = "2021"
description = "Pure PE (Portable Executable) byte parsing: no OS API, no dependencies."

[dependencies]
```

- [ ] **Step 3: Write the failing test**

Create `rust/crates/vfs-pe/src/lib.rs` containing ONLY this test module for now, so the test names resolve against functions that do not yet exist:

```rust
//! Pure PE byte parsing.

#[cfg(test)]
mod tests {
    use super::*;

    /// A 64-byte buffer starting "MZ" is the minimum this predicate accepts.
    /// It is a cheap gate, not validation: `build_image` does the real checks.
    #[test]
    fn mz_magic_and_minimum_length_gate_the_image() {
        let mut buf = vec![0u8; 0x40];
        buf[0] = b'M';
        buf[1] = b'Z';
        assert!(pe_looks_like_image(&buf));

        assert!(!pe_looks_like_image(&buf[..0x3F]), "under 0x40 is rejected");

        buf[0] = b'X';
        assert!(!pe_looks_like_image(&buf), "wrong magic is rejected");
    }

    /// Classification is by file name only and is case- and path-insensitive,
    /// because import tables spell system DLLs inconsistently.
    ///
    /// The backslash case is the one that matters here: it must hold on Linux
    /// too, which is why the implementation splits separators explicitly instead
    /// of asking `std::path` — `Path::file_name()` would return the whole string
    /// on a non-Windows host and classify a system DLL as a game-local one.
    #[test]
    fn system_dll_classification_ignores_case_and_directory() {
        assert!(is_system_import_dll("KERNEL32.dll"));
        assert!(is_system_import_dll("kernel32.dll"));
        assert!(is_system_import_dll("C:\\Windows\\System32\\kernel32.dll"));
        assert!(is_system_import_dll("System32/kernel32.dll"));
        assert!(!is_system_import_dll("steam_api64.dll"));
        assert!(!is_system_import_dll("C:\\game\\steam_api64.dll"));
    }

    /// A truncated buffer must return Err, never panic: `build_image` is the
    /// first thing that touches attacker-influenced bytes on the staging path.
    #[test]
    fn build_image_rejects_a_truncated_header_without_panicking() {
        assert!(build_image(&[]).is_err());
        assert!(build_image(&[b'M', b'Z']).is_err());

        let mut buf = vec![0u8; 0x40];
        buf[0] = b'M';
        buf[1] = b'Z';
        // e_lfanew points past the end of the buffer.
        buf[0x3C..0x40].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        assert!(build_image(&buf).is_err());
    }

    /// An import directory of zero yields no names rather than an error.
    #[test]
    fn import_dll_names_is_empty_when_there_is_no_import_directory() {
        let img = vec![0u8; 0x400];
        assert!(import_dll_names(&img, 0x80).is_empty());
    }
}
```

- [ ] **Step 4: Run the test to verify it fails**

Run: `cargo test -p vfs-pe`
Expected: FAIL — `cannot find function pe_looks_like_image in this scope` (and the same for the other three).

- [ ] **Step 5: Move the implementation in**

Prepend to `rust/crates/vfs-pe/src/lib.rs`, above the test module. Copy the bodies **verbatim** from their current homes — do not retype or "improve" them; the doc comments carry hard-won reasoning (e.g. why `build_image` checks 96 and not 112 bytes) and must travel with the code:

- from `rust/crates/vfs-inject/src/map.rs`: `rd_u16`, `rd_u32`, `rd_u64` (keep private), then `is_pe32_plus`, `dd_base`, `build_image`, `apply_relocs`, `import_dll_names`, `export_rva`
- from `rust/crates/vfs-inject/src/pe.rs`: `is_system_import_dll`, `pe_looks_like_image`

**One deliberate change while moving `is_system_import_dll`.** Its current body
extracts the basename with `std::path::Path::new(&n).file_name()`, which is
**host-OS-dependent**: `\` is a path separator on Windows and an ordinary
character everywhere else, so `is_system_import_dll("C:\\Windows\\System32\\kernel32.dll")`
answers `true` on Windows and `false` on Linux. That is exactly the
host-dependence this crate exists to eliminate, and an import table is the one
place a path-shaped DLL name legitimately appears. Replace the `Path` lookup with
an explicit split on both separators, leaving every other line of the function
alone:

```rust
pub fn is_system_import_dll(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    // Not `Path::file_name()`: that treats `\` as a separator only on Windows,
    // so the same import table would classify differently per host. A PE names
    // its imports with Windows conventions no matter who reads the file, so
    // both separators are split here explicitly.
    let base = n.rsplit(['/', '\\']).next().unwrap_or(&n);
    // ...the rest of the function is unchanged: `base.starts_with("api-ms-")`
    // || `base.starts_with("ext-ms-")` || the `matches!` list, all verbatim.
}
```

`rsplit` with a char array yields at least one item for any input, so `next()`
never returns `None`; the `unwrap_or` is belt-and-braces. On Windows the output
is identical for every input the PE loader actually accepts and every caller
actually passes — this changes Linux only, from wrong to right. It is not
identical for two inputs neither caller reaches: a trailing separator
(`"kernel32.dll\\"` — `Path::file_name()` strips it and yields `kernel32.dll`,
`rsplit` yields `""`) and a drive-relative name (`"C:kernel32.dll"` —
`Path::file_name()` yields `kernel32.dll`, `rsplit` yields the whole string).
Both are unreachable here: the PE loader never emits either shape as an import
name, and `stage.rs` basenames its input before calling this function anyway.
- from `rust/crates/vfs-inject/src/lib.rs:27-30`, the wrapper, rewritten against local functions:

```rust
/// Import DLL names of a raw PE, flattening the image first.
pub fn import_dll_names_of_pe(raw: &[u8]) -> Option<Vec<String>> {
    let (img, _base, e_lfanew) = build_image(raw).ok()?;
    Some(import_dll_names(&img, e_lfanew))
}
```

Do NOT move `map_image_from_pe_bytes_local`, `ntdll_proc`, `resolve_imports*`, `find_remote_module_base`, `rpm_*`, `remote_*`. Those call Windows APIs and stay in `vfs-inject`.

`map.rs` carries `#![allow(unsafe_code)]` for its Windows half. The functions moved here contain no `unsafe`, so do **not** carry that attribute into `vfs-pe`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p vfs-pe`
Expected: PASS, 4 tests.

- [ ] **Step 7: Verify the new crate is Linux-clean**

Run: `cargo check --target x86_64-unknown-linux-gnu -p vfs-pe`
Expected: `Finished`, no errors.

- [ ] **Step 8: Commit**

```bash
git add rust/Cargo.toml rust/crates/vfs-pe
git commit -m "feat(pe): pure PE byte parsing as its own crate

A PE is a file format, not a platform. Extracted from vfs-inject so the
director can stage Windows executables from any host OS."
```

---

### Task 2: `vfs-inject` delegates to `vfs-pe`

**Files:**
- Modify: `rust/crates/vfs-inject/Cargo.toml`
- Modify: `rust/crates/vfs-inject/src/map.rs`
- Modify: `rust/crates/vfs-inject/src/pe.rs`
- Modify: `rust/crates/vfs-inject/src/lib.rs:20,27-30`

**Interfaces:**
- Consumes: every function from Task 1.
- Produces: no change to `vfs-inject`'s public surface. `vfs_inject::pe_looks_like_image`, `::is_system_import_dll`, `::import_dll_names_of_pe` and `::map_image_from_pe_bytes_local` all still resolve.

- [ ] **Step 1: Add the dependency**

In `rust/crates/vfs-inject/Cargo.toml`, under `[dependencies]`, after the `vfs-core` line:

```toml
vfs-pe = { path = "../vfs-pe" }
```

- [ ] **Step 2: Replace the moved functions with re-exports**

In `rust/crates/vfs-inject/src/map.rs`, delete the bodies of `rd_u16`, `rd_u32`, `rd_u64`, `is_pe32_plus`, `dd_base`, `build_image`, `apply_relocs`, `import_dll_names`, `export_rva`, and add near the top of the file:

```rust
// The pure parsing half of this module now lives in `vfs-pe` — a PE is a file
// format, not a platform, and the director has to stage Windows executables on
// hosts where none of the Windows API below exists. Re-exported rather than
// re-pathed at each call site so this module's internal callers, and
// `vfs-shim`, keep the spellings they already use.
// The five names re-exported here are the ones `inject.rs`, `pe.rs` and
// `lib.rs` reach through `crate::map::`. `import_dll_names` is deliberately not
// among them: after this task its only user is the test module below, which
// names `vfs_pe` directly.
pub use vfs_pe::{apply_relocs, build_image, dd_base, export_rva, is_pe32_plus};
```

These five need `pub use` specifically, because callers reach them through
`crate::map::` from outside this module: `inject.rs:27` imports `apply_relocs`,
`build_image` and `export_rva`; `pe.rs` uses
`crate::map::{build_image, apply_relocs, is_pe32_plus, dd_base}`; and `lib.rs`
uses `map::build_image`. `mod map;` is private in `lib.rs:15`, so these do not
widen `vfs-inject`'s public surface.

**`import_dll_names` is excluded on purpose, and this is a correction.** An
earlier draft of this plan re-exported it too, justified by `lib.rs:28` calling
`map::import_dll_names` — but Step 3 below replaces those very lines, deleting
that caller. Re-exporting it anyway leaves an import with no non-test user, and
`cargo build` (which does not compile test code) warns unused, failing the
clippy constraint. Do **not** resolve that with `#[allow(unused_imports)]`: a
suppression hides the signal permanently, so a future edit that makes the import
genuinely dead would go unreported. If `map.rs`'s `#[cfg(test)]` module
references `import_dll_names`, have it call `vfs_pe::import_dll_names` directly —
`vfs-pe` is a dependency, so the qualified path resolves with no import at all.

Do **not** also import `pe_looks_like_image` or `is_system_import_dll` here —
neither is referenced anywhere in `map.rs`, and an unused import fails
`cargo clippy --all-targets -- -D warnings`.

Then fix the remaining Windows-side callers in `map.rs` and `pe.rs`: `rd_u16`/`rd_u32`/`rd_u64` were private and are no longer available, so any Windows-only function that used them (`rpm_u32`, `rpm_u16`, `remote_export_dir`, `remote_proc_by_name`, `remote_proc_by_ordinal`, `resolve_imports_ex_with_bases`) must read bytes locally. Add private helpers at the top of `map.rs`:

```rust
fn rd_u16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
```

Duplicating three one-line little-endian reads is the right call over exporting them from `vfs-pe`: they are an implementation detail of byte reading, not an interface, and `vfs-pe`'s surface should stay the PE concepts. If the compiler reports them unused after your edits, delete them rather than keeping dead code.

In `rust/crates/vfs-inject/src/pe.rs`, delete `is_system_import_dll` and `pe_looks_like_image` and add:

```rust
// Moved to `vfs-pe` (pure parsing). Re-exported so `map_image_from_pe_bytes_local`
// below and `vfs-shim`'s hook path keep their existing spellings.
pub use vfs_pe::{is_system_import_dll, pe_looks_like_image};
```

- [ ] **Step 3: Simplify the `lib.rs` wrapper**

In `rust/crates/vfs-inject/src/lib.rs`, replace lines 27-30 with:

```rust
/// Import DLL names of a raw PE. Now `vfs-pe`'s, re-exposed here because
/// `vfs-shim` and the staging path have always called it at this path.
pub use vfs_pe::import_dll_names_of_pe;
```

and leave line 20's `pub use pe::{...}` alone — it still resolves, now through `pe.rs`'s re-export.

- [ ] **Step 4: Verify Windows builds and the surface is unchanged**

Run: `cargo build -p vfs-inject`
Expected: `Finished`, no errors and no warnings about unused imports.

Run: `cargo check -p vfs-shim`
Expected: `Finished`. This is the check that `vfs-shim/src/hook.rs:3848`'s `vfs_inject::pe_looks_like_image` still resolves.

- [ ] **Step 5: Run the affected test suites**

Run: `cargo test -p vfs-pe -p vfs-inject --no-fail-fast`
Expected: PASS. If any `vfs-inject` test fails, the extraction changed behaviour — stop and report; do not edit the test.

- [ ] **Step 6: Commit**

```bash
git add rust/crates/vfs-inject
git commit -m "refactor(inject): delegate PE parsing to vfs-pe

No public surface change: every moved symbol is re-exported at its original
path, so vfs-shim and the staging path keep their spellings."
```

---

### Task 3: `overlay_layer_dir` moves to `vfs-provider`

This severs `vfs-director` → `vfs-shim`, and with it `retour` and `libudis86-sys` — a C x86 disassembler reached through a two-line path helper.

**Files:**
- Modify: `rust/crates/vfs-provider/src/path.rs`
- Modify: `rust/crates/vfs-shim/src/overlay.rs:12-29`
- Modify: `rust/crates/vfs-director/src/lib.rs:42`
- Modify: `rust/crates/vfs-director/Cargo.toml`

**Interfaces:**
- Consumes: `vfs_provider::RootId` (already defined at `path.rs:7`).
- Produces: `vfs_provider::overlay_layer_dir(overlay_root: &Path, root: RootId) -> PathBuf`, re-exported unchanged as `vfs_shim::overlay_layer_dir`, `vfs_director::overlay_layer_dir` and `vfs_embed::overlay_layer_dir`.

- [ ] **Step 1: Write the failing test**

Append to `rust/crates/vfs-provider/src/path.rs`:

```rust
#[cfg(test)]
mod overlay_layer_dir_tests {
    use super::*;

    /// The naming scheme is `root-<n>` under the overlay root, and it is the
    /// contract between two processes that never talk to each other: the shim
    /// writes here and a host-side session mounts the same directory. A change
    /// to this string is a change to that contract.
    #[test]
    fn layer_dir_is_root_n_under_the_overlay_root() {
        let base = std::path::Path::new("/tmp/ov");
        assert_eq!(overlay_layer_dir(base, RootId::DEFAULT), base.join("root-0"));
        assert_eq!(overlay_layer_dir(base, RootId(1)), base.join("root-1"));
        assert_eq!(overlay_layer_dir(base, RootId(42)), base.join("root-42"));
    }

    /// Distinct roots never share a layer directory — that separation is the
    /// whole reason the helper takes a RootId.
    #[test]
    fn distinct_roots_get_distinct_directories() {
        let base = std::path::Path::new("/tmp/ov");
        assert_ne!(overlay_layer_dir(base, RootId(0)), overlay_layer_dir(base, RootId(1)));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vfs-provider overlay_layer_dir`
Expected: FAIL — `cannot find function overlay_layer_dir in this scope`.

- [ ] **Step 3: Move the function in**

Add to `rust/crates/vfs-provider/src/path.rs`, above the test module. Carry the existing doc comment from `vfs-shim/src/overlay.rs:12-26` verbatim — it explains why the function is public at all — and add a line recording why it lives here now:

```rust
use std::path::{Path, PathBuf};

/// The physical subdirectory an overlay rooted at `overlay_root` uses for
/// `root`'s writes — `Overlay::root_dir` calls this too, so it is the one
/// place the naming scheme is defined.
///
/// Exposed because the shim's local overlay is not the only thing that reads
/// this directory: a host-side session can separately mount a read layer
/// (e.g. a `DiskProvider`) over the same physical directory so the director
/// sees what the overlay writes, without the shim and the director ever
/// talking to each other about it — the filesystem is the shared state. That
/// caller needs the exact subtree the overlay actually uses, not a re-derived
/// or hardcoded guess at it.
///
/// **It lives in `vfs-provider` rather than in the shim** because the director
/// needs it and must not depend on Windows code to get it. Reaching it through
/// `vfs-shim` pulled `retour` — and therefore the C x86 disassembler
/// `libudis86-sys` — into the kernel's dependency graph, for two lines of path
/// joining. `vfs-provider` defines the `RootId` in the signature and has no
/// dependencies of its own, so the helper adds no edge anywhere.
pub fn overlay_layer_dir(overlay_root: &Path, root: RootId) -> PathBuf {
    overlay_root.join(format!("root-{}", root.0))
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p vfs-provider overlay_layer_dir`
Expected: PASS, 2 tests.

- [ ] **Step 5: Re-export from `vfs-shim`**

In `rust/crates/vfs-shim/src/overlay.rs`, delete the function and its doc comment (lines 12-29) and put in its place:

```rust
// Moved to `vfs-provider` so the director can reach it without depending on
// this crate — that edge pulled `retour`/`libudis86-sys` into the kernel's
// graph. Re-exported because this crate's own `Overlay::root_dir`, `engine.rs`
// and eleven integration tests call it at this path.
pub use vfs_provider::overlay_layer_dir;
```

Check the file's own imports: `RootId` at line 10 comes from `vfs_redirect`, and `Path`/`PathBuf` at line 6 from `std::path`. If either is now unused in this file, remove it from the `use` list; if still used by other code in the file, leave it.

- [ ] **Step 6: Point `vfs-director` at the new home and drop the dependency**

In `rust/crates/vfs-director/src/lib.rs`, change line 42 from `pub use vfs_shim::overlay_layer_dir;` to:

```rust
pub use vfs_provider::overlay_layer_dir;
```

In `rust/crates/vfs-director/Cargo.toml`, delete the `vfs-shim = { path = "../vfs-shim" }` line from `[dependencies]`. Leave the one in `[dev-dependencies]` for now — Task 5 handles dev-dependencies.

- [ ] **Step 7: Verify the edge is gone and Windows still builds**

Run: `cargo tree -p vfs-director -e normal | grep -c libudis86`
Expected: `0`. (Note: `grep -c` exits 1 when the count is 0; that is the success case here.)

**`-e normal` is load-bearing — do not drop it.** Default `cargo tree` includes
dev-dependencies, and `vfs-shim` stays a `vfs-director` **dev**-dependency until
Task 5, so the default command reports 2 here via
`libudis86-sys <- retour <- vfs-shim [dev-dependencies] <- vfs-director`. That is
expected and is not a failure. The property this task delivers is that the kernel
*library* graph is clean, and dev-dependencies do not participate in
`cargo check -p vfs-director`. Even after Task 5 gates them, the default command
still shows the edge on a Windows host, because
`[target.'cfg(windows)'.dev-dependencies]` resolves there.

Run: `cargo build -p vfs-director -p vfs-shim -p vfs-embed`
Expected: `Finished`. `vfs-embed/src/session.rs:394` calls `vfs_shim::overlay_layer_dir` and must still resolve through the re-export.

- [ ] **Step 8: Run the affected suites**

Run: `cargo test -p vfs-provider -p vfs-director --no-fail-fast`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add rust/crates/vfs-provider rust/crates/vfs-shim rust/crates/vfs-director
git commit -m "refactor(provider): own overlay_layer_dir, severing director -> shim

A two-line path helper was reaching the director through vfs-shim and pulling
retour + libudis86-sys (a C x86 disassembler) into the kernel's dependency
graph. vfs-provider defines the RootId in its signature and has no deps."
```

---

### Task 4: `vfs-director` stages PEs via `vfs-pe`

**Files:**
- Modify: `rust/crates/vfs-director/src/stage.rs:358,382,392,425`
- Modify: `rust/crates/vfs-director/Cargo.toml`

**Interfaces:**
- Consumes: `vfs_pe::{pe_looks_like_image, import_dll_names_of_pe, is_system_import_dll}` from Task 1.
- Produces: no surface change.

- [ ] **Step 1: Add `vfs-pe`, drop `vfs-inject`**

In `rust/crates/vfs-director/Cargo.toml` `[dependencies]`, delete the `vfs-inject = { path = "../vfs-inject" }` line and add:

```toml
# Pure PE parsing for staging. Was reached through `vfs-inject`, which is
# Windows-only for its injection half; the parsing half is a file format.
vfs-pe = { path = "../vfs-pe" }
```

- [ ] **Step 2: Re-point the four call sites**

In `rust/crates/vfs-director/src/stage.rs`, replace `vfs_inject::` with `vfs_pe::` at all four sites — lines 358, 382, 392 and 425:

```rust
if !vfs_pe::pe_looks_like_image(&exe_bytes) {
```
```rust
let Some(imports) = vfs_pe::import_dll_names_of_pe(&pe) else {
```
```rust
if seen.contains(&key) || vfs_pe::is_system_import_dll(&base) {
```
```rust
if !vfs_pe::pe_looks_like_image(&bytes) {
```

Confirm no others were added since: `grep -rn "vfs_inject" rust/crates/vfs-director/src/` must return nothing.

- [ ] **Step 3: Verify Windows builds and the edge is gone**

Run: `cargo build -p vfs-director`
Expected: `Finished`.

Run: `cargo tree -p vfs-director -e normal | grep -c vfs-inject`
Expected: `0`. As in Task 3, `-e normal` restricts this to the library graph;
`vfs-shim` remains a dev-dependency until Task 5 and pulls `vfs-inject` with it.

- [ ] **Step 4: Run the staging tests**

Run: `cargo test -p vfs-director --no-fail-fast`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add rust/crates/vfs-director
git commit -m "refactor(director): stage PEs via vfs-pe, severing director -> inject"
```

---

### Task 5: Gate the ring behind `cfg(windows)`

The ring is the Windows transport; on Linux, FUSE replaces it rather than running beside it. This is the last edge, so the Linux check goes green here.

> **Superseded in part:** this task, as executed, gated both `ipc` and
> `ring_dispatch`. A later fix-wave review found `ring_dispatch` has no
> Windows dependency — it imports only `vfs-protocol`, `vfs-ipc`,
> `vfs-compose` and `Director` — and ungated it, verifying both
> `cargo check -p vfs-director --all-targets` (Windows) and
> `cargo check --target x86_64-unknown-linux-gnu -p vfs-director
> --all-targets` compile clean. Only `ipc` remains `#[cfg(windows)]` in
> `rust/crates/vfs-director/src/lib.rs` today; the code excerpts, the
> "Interfaces: Produces" line, and Step 7 below describe the state as
> originally planned and implemented in this task, not the current state.
> See the corrected illustration in the design spec, §3.

**Files:**
- Modify: `rust/crates/vfs-director/src/lib.rs:25,29`
- Modify: `rust/crates/vfs-director/Cargo.toml`
- Modify: `rust/crates/vfs-director/tests/unicode_case_fold_across_the_ring.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: on Windows, `vfs_director::ipc` and `vfs_director::ring_dispatch` are unchanged and still public. On Linux they do not exist.

- [ ] **Step 1: Gate the modules**

In `rust/crates/vfs-director/src/lib.rs`, put `cfg` attributes on lines 25 and 29:

```rust
// The shared-memory ring is how the injected shim reaches this kernel on
// Windows. On Linux the kernel is reached through /dev/fuse instead, so the
// ring is not merely unavailable there — it is the wrong transport. A
// `fuse_dispatch` sibling lands in increment 2; see
// docs/superpowers/specs/2026-08-31-linux-fuse-proton-portability-design.md.
#[cfg(windows)]
pub mod ipc;
#[cfg(windows)]
pub mod ring_dispatch;
```

Keep them in their existing alphabetical positions rather than moving them together, so the diff stays small.

- [ ] **Step 2: Move `vfs-win` and `windows-sys` to Windows-only tables**

In `rust/crates/vfs-director/Cargo.toml`, delete `vfs-win = { path = "../vfs-win" }` from `[dependencies]` and add it to the existing `[target.'cfg(windows)'.dependencies]` section, which already holds `windows-sys`:

```toml
[target.'cfg(windows)'.dependencies]
# `ipc.rs` — SharedMapping + EventNotifier, the ring's OS handles.
vfs-win = { path = "../vfs-win" }
# Window + process enumeration for the load benchmark (src/bench.rs), which is
# already `#[cfg(windows)]` in code — this makes the manifest agree.
windows-sys = { version = "0.61", features = [
  "Win32_Foundation",
  "Win32_UI_WindowsAndMessaging",
  "Win32_System_Diagnostics_ToolHelp",
] }
```

- [ ] **Step 3: Move the dev-dependencies too**

Still in `rust/crates/vfs-director/Cargo.toml`, change the `[dev-dependencies]` header to `[target.'cfg(windows)'.dev-dependencies]`, keeping `vfs-zip`, `vfs-shim` and `vfs-redirect` and their comments. `vfs-redirect` depends on `vfs-win`, so leaving these unconditional would keep `cargo test -p vfs-director` broken on Linux even once the library compiles — and running the kernel's unit tests on Linux is the point of this increment, not merely compiling it.

- [ ] **Step 4: Gate the integration test**

`rust/crates/vfs-director/tests/unicode_case_fold_across_the_ring.rs` uses `vfs_redirect::{RootMap, VolumeMap}` (line 31) and `vfs_zip` (line 32), both now Windows-only dev-deps. Add as the very first line of the file, above the existing doc comment:

```rust
// Builds its vpaths with the same `RootMap` the shim uses, which is Windows-only
// — the point of the test is that both sides of the *ring* fold identically, and
// the ring is the Windows transport.
#![cfg(windows)]
```

- [ ] **Step 5: Verify Linux still compiles**

Run: `cargo check --target x86_64-unknown-linux-gnu -p vfs-director`
Expected: `Finished`, no errors.

**Corrected during implementation — do not expect a red-to-green transition
here.** An earlier draft called this "the red test" and claimed it fails before
this task. It does not: it goes green after **Task 4**, because the two failures
it can actually see are `libudis86-sys`'s C build script (removed with the
`vfs-shim` edge in Task 3) and `std::os::windows` in `vfs-inject`'s `inject.rs`
(removed in Task 4). `cargo check` **cannot see the `vfs-win` edge at all** —
`cargo check --target x86_64-unknown-linux-gnu -p vfs-win` succeeds, because
`windows-sys` emits extern declarations that type-check on any target and fail
only at link time.

So this task's real deliverables are structural, and these are the checks that
demonstrate them:

- `cargo tree -p vfs-director --target x86_64-unknown-linux-gnu | grep -cE "vfs-win|windows-sys"` → `0`
  (for the Linux target, nothing in the graph names the Windows transport).
  **`--target` is mandatory here.** Without it `cargo tree` resolves for the
  Windows host, so both `cfg(windows)` tables resolve and the same query reports
  `vfs-win` and `windows-sys` even though the gating is correct — measured, not
  hypothetical.
- the dev-dependency gating in Step 3, which is what lets
  `cargo test --target x86_64-unknown-linux-gnu -p vfs-director` get as far as
  linking instead of dying in `libudis86-sys`'s C build script

- [ ] **Step 6: Verify the kernel's unit tests actually run on Linux**

Run: `cargo test --target x86_64-unknown-linux-gnu -p vfs-director --no-run`
Expected: `Finished`. (`--no-run` because a Linux test binary cannot execute on this Windows host; CI runs it for real. Note this step links, so if it fails for want of a cross-linker rather than a Rust error, record that and rely on CI — `cargo check` in Step 5 remains the local gate.)

- [ ] **Step 7: Verify Windows is untouched**

Run: `cargo build -p vfs-director -p vfs-directord -p vfs-embed -p vfs-server`
Expected: `Finished`. These are the consumers of the gated modules — `vfs-embed/src/session.rs:9` uses `vfs_director::ipc::IpcServe`, and `vfs-directord/tests/composition.rs:335` uses `ring_dispatch::dispatch_director`.

Run: `cargo test -p vfs-director --no-fail-fast`
Expected: PASS, including `unicode_case_fold_across_the_ring` — `#![cfg(windows)]` keeps it live on Windows.

- [ ] **Step 8: Commit**

```bash
git add rust/crates/vfs-director
git commit -m "refactor(director): gate the ring behind cfg(windows)

Severs the last Windows edge. vfs-director now compiles and its unit tests
build on Linux; the ring stays exactly as it was on Windows. FUSE is the Linux
transport and lands in increment 2."
```

---

### Task 6: CI coverage and full verification

**Files:**
- Modify: `.github/workflows/ci.yml` (`rust-linux-portable` job)

**Interfaces:** none.

- [ ] **Step 1: Extend the Linux job**

In `.github/workflows/ci.yml`, replace the `rust-linux-portable` "Portable Rust crates" step with two steps:

```yaml
      - name: Portable Rust crates (compile+test on Linux)
        run: cargo test -p vfs-ipc -p vfs-protocol -p vfs-provider -p vfs-compose -p vfs-cache -p vfs-pe -p vfs-source -p vfs-zip -p xtask-descriptor
        working-directory: rust
      # The director kernel itself, which stopped being Windows-only in
      # increment 1 of the Linux port. Its own unit tests run here; its ring
      # modules and their dev-dependencies are `cfg(windows)`, so what Linux
      # builds is the kernel and the portable provider stack under it. A
      # regression here means someone reintroduced a Windows edge into the
      # kernel — see
      # docs/superpowers/specs/2026-08-31-linux-fuse-proton-portability-design.md.
      - name: Director kernel (compile+test on Linux)
        run: cargo test -p vfs-director
        working-directory: rust
```

`vfs-source` and `vfs-zip` join the first list because they have no Windows dependency and the spec names them; if either fails to build on Linux, do not fix it here — drop it from the list, record why, and report it, since that is a finding outside this plan's scope.

- [ ] **Step 2: Verify the workflow file is valid YAML**

Run: `python -c "import yaml,sys; yaml.safe_load(open('.github/workflows/ci.yml'))"` from the repository root.
Expected: no output, exit 0. If python is unavailable, skip and rely on Step 5's review.

- [ ] **Step 3: Run the full Windows suite**

Run from `rust/`: `cargo test --no-fail-fast`
Expected: the suite's pre-existing tally, with no new failures.

**Known hazard, and how to report it:** the daemon e2e tests in `vfs-directord` were observed hanging on this machine before any of this work (`vfs-fixture-escape.exe` wedged, no log output for 18 minutes, tests `scenario_toml_*` / `escape_matrix_*` / `profile_api_*` outstanding). That hang is pre-existing and unrelated to this plan. If it recurs, do NOT try to fix it inside this plan and do NOT report the suite as passing. Record which tests were outstanding, and separately confirm the crates this plan touched:

```bash
cargo test -p vfs-pe -p vfs-provider -p vfs-director -p vfs-inject -p vfs-shim --no-fail-fast
```

- [ ] **Step 4: Run the remaining Definition-of-done gates**

```bash
cargo clippy --all-targets -- -D warnings          # expect: clean
cargo check --target x86_64-unknown-linux-gnu -p vfs-director   # expect: Finished
cargo tree -p vfs-director --target x86_64-unknown-linux-gnu | grep -E "libudis86|retour|vfs-inject|vfs-win|windows-sys"   # expect: no output
```

**`--target` is mandatory on that last check and omitting it inverts the
result.** `cargo tree` resolves for the host target by default, so on this
Windows machine both `[target.'cfg(windows)'.dependencies]` and
`[target.'cfg(windows)'.dev-dependencies]` resolve and the query reports
`vfs-win`, `windows-sys`, `retour` and `libudis86-sys` even though the gating is
correct. Naming the target covers normal and dev dependencies in one query and is
the strongest form of this check.

From the repository root:

```bash
bin/regen-protocol
git diff --exit-code resources/                    # expect: no diff, exit 0
```

- [ ] **Step 5: Confirm no public surface moved**

```bash
git diff f0a55ef --stat -- rust/crates/vfs-embed rust/crates/vfs-node
```

Expected: **no output.** Neither crate is modified by this plan; any diff means something leaked and must be reverted.

- [ ] **Step 6: Commit**

```bash
git add .github/workflows/ci.yml
git commit -m "ci: cover the director kernel and vfs-pe on Linux

Makes increment 1's property enforced rather than observed: a reintroduced
Windows edge in the kernel now fails the Linux job."
```

---

## Definition of Done

Mirrors the spec's section 8. All seven must hold:

- [ ] `cargo check --target x86_64-unknown-linux-gnu -p vfs-director` succeeds
- [ ] `cargo tree -p vfs-director --target x86_64-unknown-linux-gnu` contains none of `retour`, `libudis86-sys`, `vfs-win`, `vfs-shim`, `vfs-inject`, `windows-sys`. `--target` is mandatory — without it the query resolves for the Windows host, both `cfg(windows)` tables resolve, and it reports Windows crates even when the property holds.
- [ ] `cargo test --no-fail-fast` on Windows is green, with no test edited to make it so
- [ ] `cargo clippy --all-targets -- -D warnings` is clean
- [ ] `bin/regen-protocol` produces no diff under `resources/`
- [ ] `rust-linux-portable` in CI covers `vfs-director` and `vfs-pe`
- [ ] `vfs-embed`'s and `vfs-node`'s public surfaces are byte-identical to `f0a55ef`
