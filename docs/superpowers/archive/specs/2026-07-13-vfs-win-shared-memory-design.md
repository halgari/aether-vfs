# vfs-win Shared Memory — Design Spec

**Status:** Approved-to-proceed (standing goal: drive to a working end-to-end
VFS), ready for planning.
**Date:** 2026-07-13
**Slice:** Fifth slice — the Windows platform layer's **cross-process shared
memory**: a real `CreateFileMapping`/`MapViewOfFile`-backed segment that yields a
`vfs-ipc::SharedSeg`, so a server and an injected shim in another process share
one ring + snapshot segment.
**Parent docs:** *Out-of-Process (IPC) Architecture* (§2, §8, §11), *Rust
Implementation Guide* (§2 crate table).
**Depends on:** `vfs-ipc` (for `SharedSeg::from_raw`).

---

## 1. Context & positioning

All prior crates operate on a caller-provided byte segment; in-process tests use
`OwnedSeg` (a heap buffer). For a real deployment the **server (director) and the
injected shim live in different processes** and must map the *same* physical
pages. `vfs-win` provides that: it creates/opens a Windows file-mapping section,
maps a view, and wraps the view as a `vfs-ipc::SharedSeg`. This is the bridge
from the OS-independent crates to the Windows reality — the first crate that
touches the Win32 API.

### Scope decisions

1. **Shared-memory mapping only.** Create/open a named section, map a view,
   expose it as a `SharedSeg`, unmap+close on drop. That's the whole slice.
2. **Named sections (MVP).** Cross-process rendezvous by name (a per-session
   random name). Unnamed sections + `DuplicateHandle` into a target (IPC §11,
   more secure) is **deferred** to the injection slice, which needs the handle
   anyway.
3. **`windows-sys`, localized `unsafe`.** The Win32 FFI is `unsafe`; confine it
   with `#![deny(unsafe_code)]` + localized `#[allow]` in the mapping module.
4. **Real Nt event `Notifier` is deferred.** `SpinNotifier` already works;
   swapping it in is a later optimization. This slice is memory only.

---

## 2. Scope & crate boundary

`crates/vfs-win`, stable Rust, `#![deny(unsafe_code)]` with localized allows.

### In scope

- `SharedMapping` — RAII owner of a file-mapping `HANDLE` + mapped view; `Drop`
  unmaps the view and closes the handle.
- `SharedMapping::create(name, size)` — `CreateFileMappingW` (backed by the page
  file, `INVALID_HANDLE_VALUE`) + `MapViewOfFile`.
- `SharedMapping::open(name, size)` — `OpenFileMappingW` + `MapViewOfFile` (the
  peer process).
- `SharedMapping::seg(&self) -> &vfs_ipc::SharedSeg` — the mapped view as a
  `SharedSeg` (page-aligned base ⇒ 8-aligned, satisfying the ring's atomics).
- `unique_name(prefix) -> String` — a per-session section name (no OS randomness
  crate; derive from pid + a counter/time passed in, or a `Local\` name).

### Explicitly out of scope (later slices)

- `DuplicateHandle` into a target process (injection slice).
- The real Nt event / futex `Notifier`.
- DACL / security hardening (IPC §11) beyond a `Local\`-namespaced name.
- Any ring/protocol logic (that's `vfs-ipc`); any server logic (`vfs-server`).

---

## 3. API

```rust
pub struct SharedMapping { /* handle: HANDLE, view: *mut c_void, len: usize, seg: SharedSeg */ }

impl SharedMapping {
    /// Create a new named section of `size` bytes (rounded up by the OS to a page)
    /// and map a read/write view. Fails if the name already exists is allowed
    /// (the mapping is opened) — but `create` uses CreateFileMapping which opens
    /// an existing one of the same name; callers coordinate names.
    pub fn create(name: &str, size: usize) -> std::io::Result<Self>;

    /// Open an existing named section and map a read/write view.
    pub fn open(name: &str, size: usize) -> std::io::Result<Self>;

    pub fn seg(&self) -> &vfs_ipc::SharedSeg;
    pub fn len(&self) -> usize;
}

impl Drop for SharedMapping { /* UnmapViewOfFile(view); CloseHandle(handle) */ }

// SharedMapping is Send + Sync: the mapped pages are shared memory; access is
// governed by vfs-ipc's ring protocol (same rationale as SharedSeg).
```

- Names are converted to a NUL-terminated UTF-16 buffer for the `*W` APIs.
- `MapViewOfFile` returns a 64 KB-aligned base ⇒ the `SharedSeg` atomics
  (8-aligned) are satisfied.
- `seg()` returns a `SharedSeg` constructed once at map time via
  `SharedSeg::from_raw(view_ptr, len)` (the one `unsafe`, justified: the view is
  valid for `len` bytes for the mapping's lifetime and page-aligned).

---

## 4. Error handling

Every Win32 failure becomes `std::io::Error::last_os_error()` with context (the
call returns null/`INVALID_HANDLE_VALUE`). No panics. `Drop` ignores errors from
`UnmapViewOfFile`/`CloseHandle` (best-effort cleanup).

---

## 5. Testing

Runnable on this Windows host (`cargo test`):

- **Round-trip over a real section:** `create` a mapping, `vfs_ipc::ring::init`
  on `seg()`, run a full single-threaded slot round-trip
  (`claim`/`publish`/`server_take`/`complete`/`take_response`) — proving a real
  page-file section works as a `SharedSeg`.
- **Two views alias the same section:** `create(name)` then `open(name)` (a
  second mapping of the same section in-process), write a marker through the
  first view's `seg`, read it through the second — proving cross-process sharing
  semantics (a peer that opens the name sees the same bytes).
- **`open` of a missing name errors** (`io::Error`, no panic).
- **Alignment:** assert the mapped base is 8-aligned (it is 64 KB-aligned).
- (Cross-*process* proof — a spawned child that maps the name — is deferred to
  the injection/integration slice, which spawns real processes anyway.)

---

## 6. Dependencies & toolchain

- **Toolchain:** stable Rust.
- **Dependencies:** `vfs-ipc` (path); `windows-sys = { version = "0.59",
  features = ["Win32_Foundation", "Win32_System_Memory"] }`.
- **Unsafe:** `#![deny(unsafe_code)]` with localized `#[allow(unsafe_code)]` in
  the mapping code (Win32 FFI + the one `SharedSeg::from_raw`).
- **Workspace:** add `crates/vfs-win` to `members`.

---

## 7. Out-of-scope reminders

- No handle duplication, no real Notifier, no DACLs, no ring/server logic.

*End of spec.*
