# vfs-shim NtCreateFile Hook Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `vfs-shim` crate that installs a `retour` detour on `ntdll!NtCreateFile` and redirects opens of virtualized paths to their mod backing files, proven in-process by a `std::fs` open of a non-existent virtual path returning the backing file's bytes.

**Architecture:** `ntdef` holds `#[repr(C)]` NT structs + the `NtCreateFile` fn type (no `unsafe`). `Engine` (no `unsafe`) owns a `RootMap` + snapshot bytes and answers `decide(nt_path)` by opening a `SnapshotReader` per call and delegating to `vfs_redirect::RootMap::decide`. `hook` (all `unsafe`) installs a `RawDetour`, and the hook fn reads `OBJECT_ATTRIBUTES.ObjectName`, calls `engine.decide`, and on `Redirect` reissues the open against the mod path via the trampoline. This mechanism is already validated by a passing spike (see the memory note *vfs-ntcreatefile-hook-recipe*).

**Tech Stack:** Rust (stable). `retour = { version = "0.3", default-features = false }`, `windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_System_LibraryLoader"] }`. Path deps: `vfs-redirect`, `vfs-shared`, `vfs-core`.

## Global Constraints

- Stable Rust; crate attribute `#![deny(unsafe_code)]` with localized `#[allow(unsafe_code)]` ONLY in `hook.rs`. `ntdef.rs` and `engine.rs` contain no `unsafe`.
- The hook fn must NEVER panic (a panic across `extern "system"` aborts the process) and must do NO hookable I/O — only allocation + pure logic. No `unwrap`/`expect`/indexing-that-can-panic inside the hook.
- `HANDLE` is `*mut c_void` in windows-sys 0.59 (null-check with `.is_null()`, construct with `std::ptr::null_mut()`). `NTSTATUS` is `i32`. `GetProcAddress` returns `FARPROC = Option<unsafe extern "system" fn() -> isize>`.
- `UnicodeString.length`/`maximum_length` are in BYTES; the u16 count is `length / 2`.
- Derive `Debug` on `EngineError` and `InstallError` (used in `.unwrap()`/`Result` and diagnostics). `vfs_core::PathError` and `vfs_shared::LayoutError` both derive `Debug`.
- The hook integration test lives in its OWN test file with a SINGLE `#[test]` (installing a process-global detour must not race other tests).
- Backing-source contract (from `vfs-redirect`): the snapshot stores `source` as a UTF-8 absolute Win32 path with no NT prefix; `render_nt` adds `\??\`.

---

### Task 1: Crate scaffold, workspace wiring, and `ntdef` types

**Files:**
- Create: `crates/vfs-shim/Cargo.toml`
- Create: `crates/vfs-shim/src/lib.rs`
- Create: `crates/vfs-shim/src/ntdef.rs`
- Modify: `Cargo.toml` (workspace `members`)

**Interfaces:**
- Consumes: `windows-sys` (`HANDLE`, `NTSTATUS`).
- Produces: `ntdef::{UnicodeString, ObjectAttributes, NtCreateFileFn, STATUS_UNSUCCESSFUL}` for the hook module.

- [ ] **Step 1: Add the crate to workspace members**

In root `Cargo.toml`, add `"crates/vfs-shim"` to the `members` array (alphabetical: after `crates/vfs-server`, before `crates/vfs-win`).

- [ ] **Step 2: Write `crates/vfs-shim/Cargo.toml`**

```toml
[package]
name = "vfs-shim"
version = "0.1.0"
edition = "2021"

[dependencies]
vfs-redirect = { path = "../vfs-redirect" }
vfs-shared = { path = "../vfs-shared" }
vfs-core = { path = "../vfs-core" }
retour = { version = "0.3", default-features = false }
windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_System_LibraryLoader"] }

[dev-dependencies]
# `bridge` (test-only) provides `flatten` for building snapshot fixtures.
vfs-shared = { path = "../vfs-shared", features = ["bridge"] }
```

- [ ] **Step 3: Write `crates/vfs-shim/src/ntdef.rs`**

```rust
//! Minimal `#[repr(C)]` NT type definitions used by the NtCreateFile hook.
//! No `unsafe` here — just layout-compatible structs and the fn signature.

use core::ffi::c_void;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// `STATUS_UNSUCCESSFUL` — returned only if the trampoline is somehow unset
/// (an invariant violation the hook must not panic on).
pub const STATUS_UNSUCCESSFUL: NTSTATUS = 0xC000_0001u32 as i32;

/// Layout-compatible with the NT `UNICODE_STRING`. `length`/`maximum_length`
/// are in BYTES; the u16 count is `length / 2`.
#[repr(C)]
pub struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *mut u16,
}

/// Layout-compatible with the NT `OBJECT_ATTRIBUTES`.
#[repr(C)]
pub struct ObjectAttributes {
    pub length: u32,
    pub root_directory: HANDLE,
    pub object_name: *const UnicodeString,
    pub attributes: u32,
    pub security_descriptor: *const c_void,
    pub security_qos: *const c_void,
}

/// The `ntdll!NtCreateFile` signature. `IO_STATUS_BLOCK` is left opaque
/// (`*mut c_void`) — the hook never inspects it.
pub type NtCreateFileFn = unsafe extern "system" fn(
    *mut HANDLE, // FileHandle
    u32,         // DesiredAccess
    *const ObjectAttributes,
    *mut c_void, // IoStatusBlock
    *const i64,  // AllocationSize
    u32,         // FileAttributes
    u32,         // ShareAccess
    u32,         // CreateDisposition
    u32,         // CreateOptions
    *const c_void, // EaBuffer
    u32,         // EaLength
) -> NTSTATUS;
```

- [ ] **Step 4: Write a minimal `crates/vfs-shim/src/lib.rs`**

```rust
#![deny(unsafe_code)]

//! `vfs-shim`: installs an `NtCreateFile` detour that redirects opens of
//! virtualized paths to their mod backing files (in-process for now; injection
//! is a later slice).

mod ntdef;
```

- [ ] **Step 5: Build to verify the workspace resolves and `retour` compiles on stable**

Run: `cargo build -p vfs-shim`
Expected: compiles. `retour` 0.3 + `windows-sys` resolve. Dead-code warnings on the unused `ntdef` items are acceptable at this step (they're consumed in Task 3).

If `retour` fails to compile on stable (it should not — a spike confirmed it), STOP and report.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml crates/vfs-shim/Cargo.toml crates/vfs-shim/src/lib.rs crates/vfs-shim/src/ntdef.rs
git commit -m "vfs-shim: crate scaffold, workspace wiring, ntdef NT types"
```

---

### Task 2: `Engine` (validation + decide)

**Files:**
- Create: `crates/vfs-shim/src/engine.rs`
- Modify: `crates/vfs-shim/src/lib.rs`
- Test: `crates/vfs-shim/src/engine.rs` (`#[cfg(test)]` module)

**Interfaces:**
- Consumes: `vfs_redirect::{RootMap, Decision}`, `vfs_shared::{SnapshotReader, LayoutError}`, `vfs_core::PathError`.
- Produces:
  - `Engine::new(root: &str, snapshot: Vec<u8>) -> Result<Engine, EngineError>`
  - `Engine::decide(&self, nt_path: &str) -> vfs_redirect::Decision`
  - `enum EngineError { Root(vfs_core::PathError), Snapshot(vfs_shared::LayoutError) }` (derives `Debug`)

- [ ] **Step 1: Write the failing tests**

Create `crates/vfs-shim/src/engine.rs` with the implementation stubbed out enough
to hold a `#[cfg(test)]` module — but write the tests FIRST (they won't compile
until Step 3). Put this test module in the file:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use vfs_redirect::Decision;

    fn snapshot_bytes() -> Vec<u8> {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/foo.esp".into(),
                kind: EntryKind::File,
                source: r"D:\Mods\Cool\foo.esp".into(),
                size: 10,
                mtime: 1,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    }

    #[test]
    fn new_rejects_a_bad_snapshot() {
        // Use `matches!` on the whole Result rather than `.unwrap_err()` — the
        // latter needs `Engine: Debug`, but Engine holds a `Vec<u8>` snapshot we
        // don't want dumped, so Engine intentionally does not derive Debug.
        assert!(matches!(
            Engine::new(r"C:\Games\Skyrim", vec![0u8; 4]),
            Err(EngineError::Snapshot(_))
        ));
    }

    #[test]
    fn decide_redirects_a_virtual_file() {
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        let d = engine.decide(r"\??\C:\Games\Skyrim\Data\foo.esp");
        assert_eq!(d, Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() });
    }

    #[test]
    fn decide_passes_through_outside_root() {
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        assert_eq!(engine.decide(r"\??\C:\Windows\notepad.exe"), Decision::PassThrough);
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-shim`
Expected: FAIL to compile (`Engine`, `EngineError` not defined).

- [ ] **Step 3: Implement `Engine` + `EngineError`**

Put this ABOVE the test module in `crates/vfs-shim/src/engine.rs`:

```rust
//! The redirect engine: a `RootMap` plus the snapshot bytes it resolves against.

use vfs_redirect::{Decision, RootMap};
use vfs_shared::{LayoutError, SnapshotReader};

/// Errors constructing an [`Engine`].
#[derive(Debug)]
pub enum EngineError {
    /// The managed root path could not be normalized.
    Root(vfs_core::PathError),
    /// The snapshot bytes failed layout validation.
    Snapshot(LayoutError),
}

/// Owns the redirect policy and the snapshot it resolves against.
pub struct Engine {
    map: RootMap,
    snapshot: Vec<u8>,
}

impl Engine {
    /// `root` may be NT (`\??\C:\Games\Skyrim`) or Win32 (`C:\Games\Skyrim`).
    /// The snapshot is validated eagerly so `decide` can stay infallible.
    pub fn new(root: &str, snapshot: Vec<u8>) -> Result<Self, EngineError> {
        let map = RootMap::new(root).map_err(EngineError::Root)?;
        SnapshotReader::open(&snapshot).map_err(EngineError::Snapshot)?;
        Ok(Engine { map, snapshot })
    }

    /// Decide how to handle an incoming NT open path. Fail-safe: if the snapshot
    /// somehow fails to re-open, pass through (cannot happen after `new`).
    pub fn decide(&self, nt_path: &str) -> Decision {
        match SnapshotReader::open(&self.snapshot) {
            Ok(reader) => self.map.decide(nt_path, &reader),
            Err(_) => Decision::PassThrough,
        }
    }
}
```

Then add `mod engine;` and `pub use engine::{Engine, EngineError};` to
`crates/vfs-shim/src/lib.rs` (keep `mod ntdef;`).

- [ ] **Step 4: Run to verify passing**

Run: `cargo test -p vfs-shim`
Expected: PASS (3 engine tests).

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-shim/src/engine.rs crates/vfs-shim/src/lib.rs
git commit -m "vfs-shim: Engine with eager validation and decide"
```

---

### Task 3: The `NtCreateFile` hook + `install` + integration test

**Files:**
- Create: `crates/vfs-shim/src/hook.rs`
- Modify: `crates/vfs-shim/src/lib.rs`
- Test: `crates/vfs-shim/tests/hook_redirect.rs`

**Interfaces:**
- Consumes: `ntdef::{UnicodeString, ObjectAttributes, NtCreateFileFn, STATUS_UNSUCCESSFUL}`, `engine::Engine`, `vfs_redirect::Decision`, `retour::RawDetour`, `windows-sys` (`GetModuleHandleA`, `GetProcAddress`, `HANDLE`, `NTSTATUS`).
- Produces:
  - `install(engine: Engine) -> Result<HookGuard, InstallError>`
  - `struct HookGuard` (owns the `RawDetour`; `Drop` disables the hook)
  - `enum InstallError { AlreadyInstalled, NtdllMissing, ProcMissing, Detour }` (derives `Debug`)

- [ ] **Step 1: Write the failing integration test**

Create `crates/vfs-shim/tests/hook_redirect.rs`:

```rust
//! Single-test binary: installing a process-global NtCreateFile detour must not
//! race other tests, so this hook test stands alone.

use vfs_shim::{install, Engine};

#[test]
fn hooked_open_reads_the_backing_file() {
    // Unique temp root for this run.
    let root = std::env::temp_dir().join(format!("vfs-shim-it-{}", std::process::id()));
    let backing_dir = root.join("backing");
    std::fs::create_dir_all(&backing_dir).unwrap();
    let backing = backing_dir.join("real.esp");
    std::fs::write(&backing, b"BACKING BYTES OK").unwrap();

    // The virtual path lives directly under the root and does NOT exist on disk.
    let virtual_path = root.join("virtual.esp");
    assert!(std::fs::read(&virtual_path).is_err(), "virtual path must not pre-exist");

    // Build a snapshot mapping vpath `virtual.esp` -> the backing file's abs path.
    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let source = backing.to_str().unwrap().to_string();
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "virtual.esp".into(),
                kind: EntryKind::File,
                source: source.into(),
                size: 16,
                mtime: 1,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    // Root as a Win32 path (RootMap::new accepts it); NtCreateFile will see the
    // `\??\` NT form and RootMap matches component-wise.
    let root_str = root.to_str().unwrap();
    let engine = Engine::new(root_str, snapshot).unwrap();

    let _guard = install(engine).expect("hook install");

    // Open the VIRTUAL path — the hook redirects to the backing file.
    let content = std::fs::read_to_string(&virtual_path).expect("redirected open");
    assert_eq!(content, "BACKING BYTES OK");

    // _guard drops here, disabling the hook.
}
```

This test's `Cargo.toml` needs `vfs-core` and `vfs-shared`'s `bridge` feature as
dev-deps — already configured in Task 1 (`vfs-shared` dev-dep has `bridge`;
`vfs-core` is a normal dep, usable from tests).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vfs-shim --test hook_redirect`
Expected: FAIL to compile (`install`, `Engine` re-export path — `install` not defined yet).

- [ ] **Step 3: Implement `crates/vfs-shim/src/hook.rs`**

All `unsafe` is confined to this file. This is the validated spike code wired to
`Engine`. Follow it exactly:

```rust
//! The NtCreateFile detour. ALL `unsafe` in the crate lives here.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::sync::OnceLock;

use retour::RawDetour;
use vfs_redirect::Decision;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

use crate::engine::Engine;
use crate::ntdef::{NtCreateFileFn, ObjectAttributes, UnicodeString, STATUS_UNSUCCESSFUL};

/// Errors installing the hook.
#[derive(Debug)]
pub enum InstallError {
    AlreadyInstalled,
    NtdllMissing,
    ProcMissing,
    Detour,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();
// Set once, before the detour is enabled; only read from the hook thereafter.
static mut TRAMPOLINE: Option<NtCreateFileFn> = None;

/// Keeps the detour alive; dropping it disables the hook.
pub struct HookGuard {
    _detour: RawDetour,
}

/// Install the NtCreateFile detour backed by `engine`. Idempotent-guarded: a
/// second call returns `AlreadyInstalled`.
pub fn install(engine: Engine) -> Result<HookGuard, InstallError> {
    ENGINE.set(engine).map_err(|_| InstallError::AlreadyInstalled)?;

    // SAFETY: standard ntdll lookup + detour install. `hook` matches the
    // NtCreateFile ABI (`ntdef::NtCreateFileFn`). Trampoline is stored before
    // the detour is enabled, so the hook always observes `Some`.
    unsafe {
        let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
        if ntdll.is_null() {
            return Err(InstallError::NtdllMissing);
        }
        let proc = GetProcAddress(ntdll, b"NtCreateFile\0".as_ptr())
            .ok_or(InstallError::ProcMissing)?;
        let target = proc as *const ();
        let detour =
            RawDetour::new(target, hook as *const ()).map_err(|_| InstallError::Detour)?;
        TRAMPOLINE = Some(core::mem::transmute::<*const (), NtCreateFileFn>(
            detour.trampoline() as *const (),
        ));
        detour.enable().map_err(|_| InstallError::Detour)?;
        Ok(HookGuard { _detour: detour })
    }
}

/// The detour. Must never panic (a panic across `extern "system"` aborts) and
/// must do no hookable I/O.
unsafe extern "system" fn hook(
    file_handle: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    alloc: *const i64,
    attrs: u32,
    share: u32,
    disp: u32,
    opts: u32,
    ea: *const c_void,
    ealen: u32,
) -> NTSTATUS {
    // Invariant: TRAMPOLINE is Some once the detour is enabled.
    let tramp = match TRAMPOLINE {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };

    if let Some(engine) = ENGINE.get() {
        if !oa.is_null() {
            let oa_ref = &*oa;
            // MVP: only fully-qualified opens (no RootDirectory-relative).
            if oa_ref.root_directory.is_null() && !oa_ref.object_name.is_null() {
                let us = &*oa_ref.object_name;
                if !us.buffer.is_null() {
                    let units = core::slice::from_raw_parts(us.buffer, us.length as usize / 2);
                    let path = String::from_utf16_lossy(units);
                    if let Decision::Redirect { target_nt } = engine.decide(&path) {
                        // Buffers live across the synchronous trampoline call.
                        let mut wbuf: Vec<u16> = target_nt.encode_utf16().collect();
                        let byte_len = (wbuf.len() * 2) as u16;
                        let new_us = UnicodeString {
                            length: byte_len,
                            maximum_length: byte_len,
                            buffer: wbuf.as_mut_ptr(),
                        };
                        let new_oa = ObjectAttributes {
                            length: oa_ref.length,
                            root_directory: core::ptr::null_mut(),
                            object_name: &new_us,
                            attributes: oa_ref.attributes,
                            security_descriptor: oa_ref.security_descriptor,
                            security_qos: oa_ref.security_qos,
                        };
                        let status = tramp(
                            file_handle, access, &new_oa, iosb, alloc, attrs, share, disp, opts,
                            ea, ealen,
                        );
                        drop(wbuf);
                        return status;
                    }
                }
            }
        }
    }

    tramp(file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen)
}
```

Then add to `crates/vfs-shim/src/lib.rs`: `mod hook;` and
`pub use hook::{install, HookGuard, InstallError};` (keep the existing `mod
ntdef;`, `mod engine;`, and `pub use engine::...`).

- [ ] **Step 4: Run the integration test**

Run: `cargo test -p vfs-shim --test hook_redirect`
Expected: PASS — `hooked_open_reads_the_backing_file` reads `"BACKING BYTES OK"`.

If it fails, DO NOT hack the test. STOP and report the exact failure. Likely
causes to check (do not guess-fix): a `windows-sys` signature mismatch for
`GetModuleHandleA`/`GetProcAddress` (adapt to the compiler error only), or the
detour target address. The spike (memory: *vfs-ntcreatefile-hook-recipe*) proved
this exact shape works, so a compile error is a signature nuance, not a design
flaw.

- [ ] **Step 5: Run all vfs-shim tests together**

Run: `cargo test -p vfs-shim`
Expected: engine unit tests + the hook integration test all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/vfs-shim/src/hook.rs crates/vfs-shim/src/lib.rs crates/vfs-shim/tests/hook_redirect.rs
git commit -m "vfs-shim: NtCreateFile detour redirecting opens to backing files"
```

---

### Task 4: Verification sweep

**Files:** none (verification only).

- [ ] **Step 1: Full workspace test**

Run: `cargo test --workspace`
Expected: all crates green, including `vfs-shim`.

- [ ] **Step 2: Warning check**

Run: `cargo build --workspace 2>&1 | rg -i "warning" || echo "no warnings"`
Expected: `no warnings`.

- [ ] **Step 3: Unsafe audit**

Run: confirm `crates/vfs-shim/src/lib.rs` is `#![deny(unsafe_code)]`, `ntdef.rs`
and `engine.rs` contain no `unsafe`, and every `unsafe` in `hook.rs` is under the
file's `#![allow(unsafe_code)]` with the `// SAFETY:` note on `install`.
Expected: all `unsafe` confined to `hook.rs`.

- [ ] **Step 4: Commit Cargo.lock**

```bash
git add Cargo.lock
git commit -m "vfs-shim: update Cargo.lock for retour dependency"
```

---

## Self-Review Notes

- **Spec coverage:** `ntdef` types (Task 1), `Engine`/`EngineError` (Task 2), `hook`/`install`/`HookGuard`/`InstallError` (Task 3), in-process redirect proof (Task 3 integration test), single-test isolation for the global hook (Task 3 is its own `tests/` binary), workspace wiring + deps + localized unsafe (Task 1 + Global Constraints + Task 4 audit).
- **Derives:** `EngineError` and `InstallError` derive `Debug` (used via `.unwrap()`/`.expect()`/`matches!`). `Decision` already derives `Debug, PartialEq` (from `vfs-redirect`) for the `assert_eq!` in Task 2.
- **Type consistency:** hook fn signature matches `ntdef::NtCreateFileFn` exactly (same param order/types); `engine.decide(&str) -> Decision` matches the hook's use; `HANDLE` null handling via `.is_null()`/`null_mut()` per windows-sys 0.59.
- **No-panic in hook:** the hook uses `match`/`if let` only — no `unwrap`/`expect`/`[]` indexing. `install` (not the hook) may use `?`/`ok_or`.
- **Spike parity:** Task 3's hook is the exact structure the passing spike used, with the hardcoded redirect replaced by `engine.decide`.
