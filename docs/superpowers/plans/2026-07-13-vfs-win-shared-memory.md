# vfs-win Shared Memory Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `vfs-win` crate that creates/opens a real Windows file-mapping section and exposes the mapped view as a `vfs-ipc::SharedSeg`, so a server and an injected shim in separate processes share one ring + snapshot segment.

**Architecture:** A single RAII type `SharedMapping` owns a file-mapping `HANDLE` plus a mapped view pointer. `create` calls `CreateFileMappingW`(page-file backed) + `MapViewOfFile`; `open` calls `OpenFileMappingW` + `MapViewOfFile`. The view pointer + length are wrapped once into a `vfs_ipc::SharedSeg` via `SharedSeg::from_raw`. `Drop` unmaps the view and closes the handle. All Win32 FFI is confined to one module behind localized `#[allow(unsafe_code)]`.

**Tech Stack:** Rust (stable), `windows-sys` 0.59 (`Win32_Foundation`, `Win32_System_Memory`), `vfs-ipc` (path dep, for `SharedSeg`).

## Global Constraints

- MVP is 64-bit only.
- Crate attribute: `#![deny(unsafe_code)]` with localized `#[allow(unsafe_code)]` only in the mapping FFI.
- No panics on Win32 failure — return `std::io::Error::last_os_error()`.
- `windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_System_Memory", "Win32_Security"] }` (the `Win32_Security` feature is required because `CreateFileMappingW` takes a `*const SECURITY_ATTRIBUTES` and is feature-gated behind it).
- Derive `Debug` on every public type; derive `PartialEq, Eq` on POD types used in `assert_eq!`.
- Names passed to `*W` APIs must be NUL-terminated UTF-16.
- Section names use the `Local\` namespace prefix supplied by the caller (the crate does not add it); tests use a `Local\`-prefixed unique name.

---

### Task 1: Crate scaffold + workspace wiring

**Files:**
- Create: `crates/vfs-win/Cargo.toml`
- Create: `crates/vfs-win/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members`)

**Interfaces:**
- Consumes: nothing.
- Produces: an empty `vfs-win` crate that builds in the workspace; `vfs_ipc` and `windows-sys` available as deps.

- [ ] **Step 1: Add the crate to the workspace members**

In root `Cargo.toml`, add `"crates/vfs-win"` to the `members` array (keep it sorted after `crates/vfs-server`).

- [ ] **Step 2: Write `crates/vfs-win/Cargo.toml`**

```toml
[package]
name = "vfs-win"
version = "0.1.0"
edition = "2021"

[dependencies]
vfs-ipc = { path = "../vfs-ipc" }
windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_System_Memory"] }
```

- [ ] **Step 3: Write a minimal `crates/vfs-win/src/lib.rs`**

```rust
#![deny(unsafe_code)]

//! Windows platform layer: cross-process shared memory backing a `vfs_ipc::SharedSeg`.

mod mapping;

pub use mapping::SharedMapping;
```

- [ ] **Step 4: Create a placeholder `crates/vfs-win/src/mapping.rs` so it compiles**

```rust
//! File-mapping-backed shared memory. All Win32 FFI is confined here.

/// RAII owner of a Windows file-mapping section and its mapped view.
pub struct SharedMapping;
```

- [ ] **Step 5: Build to verify the workspace resolves**

Run: `cargo build -p vfs-win`
Expected: compiles (dead-code warnings on the placeholder are acceptable at this step).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/vfs-win/Cargo.toml crates/vfs-win/src/lib.rs crates/vfs-win/src/mapping.rs
git commit -m "vfs-win: crate scaffold and workspace wiring"
```

---

### Task 2: `SharedMapping::create` + `seg` + `Drop`

**Files:**
- Modify: `crates/vfs-win/src/mapping.rs`
- Test: `crates/vfs-win/src/mapping.rs` (`#[cfg(test)]` module)

**Interfaces:**
- Consumes: `vfs_ipc::SharedSeg` (`SharedSeg::from_raw(ptr: *mut u8, len: usize) -> SharedSeg`, plus its `write_u32`/`read_u32` accessors).
- Produces:
  - `SharedMapping::create(name: &str, size: usize) -> std::io::Result<SharedMapping>`
  - `SharedMapping::seg(&self) -> &vfs_ipc::SharedSeg`
  - `SharedMapping::len(&self) -> usize`
  - `impl Drop for SharedMapping`
  - `SharedMapping: Send + Sync`

- [ ] **Step 1: Write the failing test**

Add to `crates/vfs-win/src/mapping.rs`. Note: `SharedSeg`'s byte accessors are
`pub(crate)` and NOT reachable from `vfs-win`; the only public `SharedSeg` API is
`from_raw`/`len`/`is_empty`. So writability + alignment are proven through the
public ring API: `vfs_ipc::ring::init` writes the header MAGIC into the mapped
pages and requires an 8-aligned base for its `AtomicU64` generation — a
successful `init` proves the view is writable AND correctly aligned.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // A unique-ish section name without an OS randomness crate: derive from the
    // current process id and a per-test discriminator.
    fn section_name(tag: &str) -> String {
        let pid = std::process::id();
        format!("Local\\vfs-win-test-{pid}-{tag}")
    }

    #[test]
    fn create_maps_a_writable_section() {
        let m = SharedMapping::create(&section_name("create"), 64 * 1024).unwrap();
        assert_eq!(m.len(), 64 * 1024);
        // Initializing a ring writes the MAGIC/geometry into the mapped view and
        // requires an 8-aligned base for its atomics; success proves the section
        // is writable and correctly aligned.
        vfs_ipc::ring::init(m.seg(), 4, 256).unwrap();
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vfs-win create_maps_a_writable_section`
Expected: FAIL to compile (`create`, `seg`, `len` not defined).

- [ ] **Step 3: Implement `SharedMapping` with `create`, `seg`, `len`, `Drop`**

Replace the contents of `crates/vfs-win/src/mapping.rs` (keep the `#[cfg(test)]` module from Step 1 at the bottom):

```rust
//! File-mapping-backed shared memory. All Win32 FFI is confined here.

use std::io;

use vfs_ipc::SharedSeg;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

/// RAII owner of a Windows file-mapping section and its mapped read/write view.
///
/// The mapped view is exposed as a [`SharedSeg`] so the OS-independent ring and
/// snapshot code operate on real cross-process shared memory.
pub struct SharedMapping {
    handle: HANDLE,
    view: *mut core::ffi::c_void,
    len: usize,
    seg: SharedSeg,
}

// SAFETY: the mapped pages are shared memory; all concurrent access is governed
// by the vfs-ipc ring protocol (atomics + seqlock), the same rationale that
// makes `SharedSeg` itself `Send + Sync`.
#[allow(unsafe_code)]
unsafe impl Send for SharedMapping {}
#[allow(unsafe_code)]
unsafe impl Sync for SharedMapping {}

impl SharedMapping {
    /// Create a new named page-file-backed section of at least `size` bytes and
    /// map a read/write view. If a section of `name` already exists,
    /// `CreateFileMappingW` opens it (callers coordinate names to avoid this).
    pub fn create(name: &str, size: usize) -> io::Result<Self> {
        let wide = to_wide(name);
        let (size_high, size_low) = split_size(size)?;
        // SAFETY: FFI. INVALID_HANDLE_VALUE => page-file backing; wide is a valid
        // NUL-terminated UTF-16 pointer living for the call.
        #[allow(unsafe_code)]
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                core::ptr::null(),
                PAGE_READWRITE,
                size_high,
                size_low,
                wide.as_ptr(),
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Self::map_view(handle, size)
    }

    /// Map a read/write view of `handle` (which the constructor owns from here
    /// on) and wrap it as a `SharedSeg`. On failure the handle is closed.
    fn map_view(handle: HANDLE, size: usize) -> io::Result<Self> {
        // SAFETY: FFI. `handle` is a valid mapping handle; mapping the whole
        // section (offset 0, `size` bytes).
        #[allow(unsafe_code)]
        let view: MEMORY_MAPPED_VIEW_ADDRESS =
            unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        if view.Value.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: FFI. `handle` is valid; best-effort cleanup.
            #[allow(unsafe_code)]
            unsafe {
                CloseHandle(handle);
            }
            return Err(err);
        }
        let ptr = view.Value as *mut u8;
        // SAFETY: `ptr` is valid for `size` bytes for this mapping's lifetime and
        // is page-aligned (64 KB), satisfying the ring's 8-byte atomics.
        #[allow(unsafe_code)]
        let seg = unsafe { SharedSeg::from_raw(ptr, size) };
        Ok(Self {
            handle,
            view: view.Value,
            len: size,
            seg,
        })
    }

    /// The mapped view as a `SharedSeg`.
    pub fn seg(&self) -> &SharedSeg {
        &self.seg
    }

    /// The mapped length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapping is zero-length (never true for a live mapping).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for SharedMapping {
    fn drop(&mut self) {
        // SAFETY: FFI. `view`/`handle` were produced by MapViewOfFile /
        // CreateFileMappingW and are unmapped/closed exactly once here.
        #[allow(unsafe_code)]
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view });
            CloseHandle(self.handle);
        }
    }
}

/// Convert a `&str` to a NUL-terminated UTF-16 buffer for the `*W` APIs.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

/// Split a `usize` size into the (high, low) 32-bit halves the mapping APIs take.
fn split_size(size: usize) -> io::Result<(u32, u32)> {
    let size = size as u64;
    Ok(((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32))
}
```

> Implementer note on `MapViewOfFile`'s return type: in `windows-sys` 0.59 it returns `MEMORY_MAPPED_VIEW_ADDRESS` (a struct with a single `Value: *mut c_void` field), and `UnmapViewOfFile` takes that same struct by value. The code above reflects that. If a `cargo build` shows the signature differs in the resolved patch version (e.g. it returns a raw `*mut c_void`), adapt: use the pointer directly and pass it to `UnmapViewOfFile`. Verify against the actual compiler error, do not guess.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p vfs-win create_maps_a_writable_section`
Expected: PASS.

`SharedSeg::from_raw(ptr: *mut u8, len: usize) -> Self` is `unsafe` and public
(confirmed in `crates/vfs-ipc/src/seg.rs`); `vfs_ipc::ring::init(seg, num_slots,
payload_cap) -> Result<Geom, IpcError>` is public. If either signature differs
from what this task assumes, STOP and report rather than adding public API to
`vfs-ipc`.

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-win/src/mapping.rs
git commit -m "vfs-win: SharedMapping::create with mapped SharedSeg view"
```

---

### Task 3: `SharedMapping::open` + aliasing test + missing-name error test

**Files:**
- Modify: `crates/vfs-win/src/mapping.rs`
- Test: `crates/vfs-win/src/mapping.rs` (`#[cfg(test)]` module)

**Interfaces:**
- Consumes: `to_wide`, `split_size`, `SharedMapping::map_view` from Task 2.
- Produces: `SharedMapping::open(name: &str, size: usize) -> std::io::Result<SharedMapping>`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block:

```rust
#[test]
fn open_aliases_the_same_section() {
    let name = section_name("alias");
    let creator = SharedMapping::create(&name, 64 * 1024).unwrap();
    // Creator writes the ring MAGIC + geometry into the shared pages.
    let geom_created = vfs_ipc::ring::init(creator.seg(), 4, 256).unwrap();
    // A second mapping of the SAME section sees those bytes: ring::open validates
    // the MAGIC the creator wrote and recovers the identical geometry.
    let opener = SharedMapping::open(&name, 64 * 1024).unwrap();
    let geom_opened = vfs_ipc::ring::open(opener.seg()).unwrap();
    assert_eq!(geom_created, geom_opened);
}

#[test]
fn open_missing_section_errors() {
    let name = section_name("does-not-exist-xyz");
    let err = SharedMapping::open(&name, 64 * 1024);
    assert!(err.is_err());
}
```

> `Geom` derives `PartialEq, Eq` (added in the vfs-ipc slice), so `assert_eq!` on
> two `Geom` values compiles. `vfs_ipc::ring::open(seg) -> Result<Geom, IpcError>`
> is public. If `Geom` lacks `PartialEq`, STOP and report — do not weaken the test
> to skip the geometry comparison.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-win open_`
Expected: FAIL to compile (`open` not defined).

- [ ] **Step 3: Implement `open`**

Add to `impl SharedMapping` in `crates/vfs-win/src/mapping.rs`, and add `OpenFileMappingW` to the `windows_sys::Win32::System::Memory` import list:

```rust
    /// Open an existing named section and map a read/write view.
    pub fn open(name: &str, size: usize) -> io::Result<Self> {
        let wide = to_wide(name);
        // SAFETY: FFI. `wide` is a valid NUL-terminated UTF-16 pointer for the
        // call; FALSE (0) => the mapped view handle is not inheritable.
        #[allow(unsafe_code)]
        let handle = unsafe {
            OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide.as_ptr())
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Self::map_view(handle, size)
    }
```

Update the import:

```rust
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
    FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
```

> `OpenFileMappingW`'s second argument (`binherithandle`) is a `BOOL` (i32 in windows-sys); pass `0`. If the resolved signature wants a different integer type, match it against the compiler error.

- [ ] **Step 4: Run to verify passing**

Run: `cargo test -p vfs-win`
Expected: PASS (all three tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-win/src/mapping.rs
git commit -m "vfs-win: SharedMapping::open with cross-view aliasing"
```

---

### Task 4: Ring round-trip over a real section (integration test)

**Files:**
- Create: `crates/vfs-win/tests/ring_over_section.rs`

**Interfaces:**
- Consumes: `vfs_win::SharedMapping`; the public `vfs-ipc` ring API (`vfs_ipc::ring::init`, `vfs_ipc::{RingClient, RingServer, SpinNotifier}`, `vfs_ipc::layout::OP_GETATTR`). These exact names and the `thread::scope` pattern are copied from `crates/vfs-server/tests/e2e.rs`.
- Produces: an end-to-end test proving a `SharedMapping`-backed `SharedSeg` drives a full ring request→response round-trip.

- [ ] **Step 1: Write the integration test**

Create `crates/vfs-win/tests/ring_over_section.rs`. This mirrors the `thread::scope`
client↔server pattern in `crates/vfs-server/tests/e2e.rs`, but the `SharedSeg`
comes from `SharedMapping::create(...).seg()` instead of `OwnedSeg`, and the
server handler is a trivial echo (no `vfs-server` dependency — `vfs-win` depends
only on `vfs-ipc`):

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use vfs_ipc::layout::OP_GETATTR;
use vfs_ipc::ring::init;
use vfs_ipc::{RingClient, RingServer, SpinNotifier};
use vfs_win::SharedMapping;

fn section_name(tag: &str) -> String {
    let pid = std::process::id();
    format!("Local\\vfs-win-ringtest-{pid}-{tag}")
}

#[test]
fn ring_round_trip_over_real_section() {
    let mapping = SharedMapping::create(&section_name("ring"), 64 * 1024).unwrap();
    init(mapping.seg(), 4, 4096).unwrap();
    let seg = mapping.seg();
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        // Server thread: echo each request's payload back until stopped and idle.
        scope.spawn(|| {
            let ring = RingServer::new(seg, SpinNotifier).unwrap();
            loop {
                match ring.serve_one(|req| (0, req.payload.clone())) {
                    Ok(true) => {}
                    Ok(false) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Client (this thread): submit a request over the real shared section.
        let client = RingClient::new(seg, SpinNotifier).unwrap();
        let resp = client.submit(OP_GETATTR, 0, b"hello-shared-memory").unwrap();
        assert_eq!(resp.status, 0);
        assert_eq!(resp.payload, b"hello-shared-memory");

        stop.store(true, Ordering::Relaxed);
    });
}
```

`mapping` is declared before `thread::scope` so it outlives both threads; `seg`
is a `&SharedSeg` (which is `Sync`), so both closures may share it. This is the
same lifetime shape as the vfs-server e2e test.

- [ ] **Step 2: Run the test**

Run: `cargo test -p vfs-win --test ring_over_section`
Expected: PASS.

If any ring API name differs from the above, mirror `crates/vfs-server/tests/e2e.rs`
verbatim (it is known-good) and only change the `seg` source to `SharedMapping`.

- [ ] **Step 3: Commit**

```bash
git add crates/vfs-win/tests/ring_over_section.rs
git commit -m "vfs-win: integration test driving a ring over a real file-mapping section"
```

---

### Task 5: Verification sweep

**Files:** none (verification only).

- [ ] **Step 1: Full workspace test**

Run: `cargo test --workspace`
Expected: all crates green, including the new `vfs-win` tests.

- [ ] **Step 2: Warning check**

Run: `cargo build --workspace 2>&1 | rg -i "warning" || echo "no warnings"`
Expected: `no warnings` (fix any warnings in `vfs-win` before proceeding).

- [ ] **Step 3: Unsafe audit**

Run: read every `#[allow(unsafe_code)]` site in `crates/vfs-win/src/mapping.rs` and confirm each has a `// SAFETY:` justification and is confined to the mapping module. `lib.rs` must remain `#![deny(unsafe_code)]`.
Expected: all unsafe confined to `mapping.rs`, each justified.

- [ ] **Step 4: Commit Cargo.lock**

```bash
git add Cargo.lock
git commit -m "vfs-win: update Cargo.lock for windows-sys dependency"
```

---

## Self-Review Notes

- **Spec coverage:** `create` (Task 2), `open` (Task 3), `seg`/`len` (Task 2), `Drop` (Task 2), `Send`/`Sync` (Task 2), alignment (Task 2 test), aliasing (Task 3 test), missing-name error (Task 3 test), ring round-trip over a real section (Task 4), workspace wiring (Task 1), stable toolchain + `windows-sys` features + localized unsafe (Global Constraints + Task 5 audit). `unique_name` from the spec is realized as the per-test `section_name` helper — the spec listed it as optional ("or a `Local\` name"); production name generation is the director's job in a later slice, so no standalone public helper is needed now (YAGNI).
- **Derives:** `SharedMapping` owns a raw pointer/handle and is not compared or `Debug`-printed in tests, so no `Debug`/`PartialEq` derive is required (the recurring derive-bug pattern doesn't apply — no `.unwrap_err()` on it, no `assert_eq!` of it). All `assert_eq!` in tests compare `u32`/`usize`, which already implement the needed traits.
- **windows-sys uncertainty:** the `MEMORY_MAPPED_VIEW_ADDRESS` return/param shape is the one real risk; Tasks 2 and 3 carry explicit "verify against the compiler error, do not guess" notes.
