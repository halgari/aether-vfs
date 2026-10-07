# Hooks: Path-Based Attribute Queries Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Hook `NtQueryAttributesFile` + `NtQueryFullAttributesFile` so `GetFileAttributesW`/`GetFileAttributesExW` see virtual files/dirs and tombstoned files appear absent — with a multi-detour install shared by future hooks.

**Architecture:** `install` now builds three detours (create + two query fns), each with its own trampoline static; `HookGuard` owns a `Vec<RawDetour>`. A shared `path_of(oa)` decodes the ObjectName; the query hooks call `Engine::query_attributes` and fill the caller's `#[repr(C)]` info struct or return `STATUS_OBJECT_NAME_NOT_FOUND`. Spike-validated.

**Tech Stack:** Rust (stable). `vfs-shim` (`#![deny(unsafe_code)]`, unsafe in `hook.rs`); `windows-sys` (test dev-dep adds `Win32_Storage_FileSystem`).

## Global Constraints

- Stable; all `unsafe` in `hook.rs`; crate root `#![deny(unsafe_code)]`.
- Hooks never panic; tolerate a null info pointer (skip fill, still return status).
- Times set to 0 (MVP); `GetFileAttributesW` reads only attribute flags, `Ex` also reads size (filled).
- Integration test is its own single-`#[test]` binary.

---

### Task 1: `ntdef` info structs/consts + `Engine::query_attributes`

**Files:**
- Modify: `crates/vfs-shim/src/ntdef.rs`
- Modify: `crates/vfs-shim/src/engine.rs`
- Test: `crates/vfs-shim/src/engine.rs` (`#[cfg(test)]`)

**Interfaces:**
- Produces: `FileBasicInformation`, `FileNetworkOpenInformation`, `NtQueryAttributesFileFn`, `NtQueryFullAttributesFileFn`, `STATUS_SUCCESS`, `FILE_ATTRIBUTE_DIRECTORY`, `FILE_ATTRIBUTE_NORMAL`; `Engine::query_attributes(nt_path) -> vfs_redirect::AttrDecision`.

- [ ] **Step 1: Add ntdef structs, consts, and fn types**

Append to `crates/vfs-shim/src/ntdef.rs`:

```rust
/// `STATUS_SUCCESS`.
pub const STATUS_SUCCESS: NTSTATUS = 0;
/// `FILE_ATTRIBUTE_DIRECTORY` / `FILE_ATTRIBUTE_NORMAL`.
pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// Layout-compatible with `FILE_BASIC_INFORMATION` (40 bytes).
#[repr(C)]
pub struct FileBasicInformation {
    pub creation_time: i64,
    pub last_access_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
    pub file_attributes: u32,
    pub _reserved: u32,
}

/// Layout-compatible with `FILE_NETWORK_OPEN_INFORMATION` (56 bytes).
#[repr(C)]
pub struct FileNetworkOpenInformation {
    pub creation_time: i64,
    pub last_access_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
    pub allocation_size: i64,
    pub end_of_file: i64,
    pub file_attributes: u32,
    pub _reserved: u32,
}

pub type NtQueryAttributesFileFn =
    unsafe extern "system" fn(*const ObjectAttributes, *mut FileBasicInformation) -> NTSTATUS;
pub type NtQueryFullAttributesFileFn =
    unsafe extern "system" fn(*const ObjectAttributes, *mut FileNetworkOpenInformation) -> NTSTATUS;
```

- [ ] **Step 2: Write the failing engine test**

Add to `crates/vfs-shim/src/engine.rs` tests:

```rust
    #[test]
    fn query_attributes_reports_virtual_file() {
        use vfs_redirect::AttrDecision;
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        assert_eq!(
            engine.query_attributes(r"\??\C:\Games\Skyrim\Data\foo.esp"),
            AttrDecision::Attributes { is_dir: false, size: 10, mtime: 1 }
        );
    }
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p vfs-shim query_attributes_reports_virtual_file`
Expected: FAIL to compile (`Engine::query_attributes` undefined).

- [ ] **Step 4: Implement `Engine::query_attributes`**

In `crates/vfs-shim/src/engine.rs`, change the import to bring in `AttrDecision`:

```rust
use vfs_redirect::{AttrDecision, Decision, RootMap};
```

Add to `impl Engine`:

```rust
    /// Answer a path-based attribute query against the snapshot. Fail-safe.
    pub fn query_attributes(&self, nt_path: &str) -> AttrDecision {
        match SnapshotReader::open(&self.snapshot) {
            Ok(reader) => self.map.query_attributes(nt_path, &reader),
            Err(_) => AttrDecision::PassThrough,
        }
    }
```

- [ ] **Step 5: Run to verify passing**

Run: `cargo test -p vfs-shim`
Expected: PASS (engine tests incl. the new one; hooks unchanged still pass).

- [ ] **Step 6: Commit**

```bash
git add crates/vfs-shim/src/ntdef.rs crates/vfs-shim/src/engine.rs
git commit -m "vfs-shim: attribute info structs + Engine::query_attributes"
```

---

### Task 2: Multi-detour install + query hooks + integration test

**Files:**
- Modify: `crates/vfs-shim/src/hook.rs` (full rewrite)
- Modify: `crates/vfs-shim/Cargo.toml` (dev-dep feature)
- Test: `crates/vfs-shim/tests/hook_attrs.rs`

**Interfaces:**
- Consumes: Task 1's ntdef types + `Engine::query_attributes`; `retour::RawDetour`; `windows-sys`.
- Produces: `install` hooking three functions; `qattr_hook`/`qfull_hook`; `HookGuard { Vec<RawDetour> }`.

- [ ] **Step 1: Add the test dev-dependency feature**

In `crates/vfs-shim/Cargo.toml`, under `[dev-dependencies]`, add (alongside the
existing `vfs-shared` bridge dev-dep):

```toml
windows-sys = { version = "0.59", features = ["Win32_Foundation", "Win32_Storage_FileSystem"] }
```

- [ ] **Step 2: Write the failing integration test**

Create `crates/vfs-shim/tests/hook_attrs.rs`:

```rust
//! Single-test binary: path-based attribute queries reflect the VFS.
use std::ffi::c_void;
use vfs_shim::{install, Engine};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileAttributesExW, GetFileAttributesW, GetFileExInfoStandard, FILE_ATTRIBUTE_DIRECTORY,
    INVALID_FILE_ATTRIBUTES, WIN32_FILE_ATTRIBUTE_DATA,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[test]
fn attribute_queries_reflect_the_vfs() {
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-shim-attrs-{pid}"));
    let backing_dir = root.join("backing");
    std::fs::create_dir_all(&backing_dir).unwrap();

    // Real files on disk under the root.
    let real = root.join("real.esp"); // not in snapshot -> pass through
    let gone = root.join("gone.esp"); // tombstoned -> hidden
    std::fs::write(&real, b"real").unwrap();
    std::fs::write(&gone, b"gone").unwrap();
    let backing = backing_dir.join("mod.esp");
    std::fs::write(&backing, vec![0u8; 1234]).unwrap();

    // Virtual paths (absent on disk).
    let vfile = root.join("mod.esp");
    let vdir = root.join("moddir");

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let e = |vpath: &str, kind: EntryKind, source: &str, size: u64| InputEntry {
            vpath: vpath.into(),
            kind,
            source: source.into(),
            size,
            mtime: 0,
        };
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![
                e("mod.esp", EntryKind::File, backing.to_str().unwrap(), 1234),
                e("moddir", EntryKind::Dir, "", 0),
                e("gone.esp", EntryKind::Tombstone, "", 0),
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    // Virtual file exists (not INVALID) and is not a directory.
    let a = unsafe { GetFileAttributesW(wide(vfile.to_str().unwrap()).as_ptr()) };
    assert_ne!(a, INVALID_FILE_ATTRIBUTES, "virtual file should have attributes");
    assert_eq!(a & FILE_ATTRIBUTE_DIRECTORY, 0, "virtual file must not be a dir");

    // Virtual dir has the DIRECTORY bit.
    let d = unsafe { GetFileAttributesW(wide(vdir.to_str().unwrap()).as_ptr()) };
    assert_ne!(d, INVALID_FILE_ATTRIBUTES);
    assert_ne!(d & FILE_ATTRIBUTE_DIRECTORY, 0, "virtual dir must be a dir");

    // Tombstoned real file is hidden.
    let g = unsafe { GetFileAttributesW(wide(gone.to_str().unwrap()).as_ptr()) };
    assert_eq!(g, INVALID_FILE_ATTRIBUTES, "tombstoned file must be hidden");

    // Non-virtual real file passes through.
    let r = unsafe { GetFileAttributesW(wide(real.to_str().unwrap()).as_ptr()) };
    assert_ne!(r, INVALID_FILE_ATTRIBUTES, "real file should pass through");

    // Full attributes report the snapshot's size.
    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileAttributesExW(
            wide(vfile.to_str().unwrap()).as_ptr(),
            GetFileExInfoStandard,
            &mut data as *mut _ as *mut c_void,
        )
    };
    assert_ne!(ok, 0, "GetFileAttributesExW should succeed for the virtual file");
    let size = ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64;
    assert_eq!(size, 1234, "reported size should match the snapshot");
}
```

Note: `INVALID_FILE_ATTRIBUTES` and `FILE_ATTRIBUTE_DIRECTORY` are exported by
`windows-sys` under `Win32::Storage::FileSystem`. If any name doesn't resolve,
check the compiler error and adapt to the exact windows-sys path/name (e.g.
`INVALID_FILE_ATTRIBUTES` may need `use windows_sys::Win32::Storage::FileSystem::*`
or a local `const INVALID_FILE_ATTRIBUTES: u32 = u32::MAX;`). Do not change what
the test verifies.

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p vfs-shim --test hook_attrs`
Expected: FAIL — the query functions aren't hooked yet, so `GetFileAttributesW`
on the (nonexistent-on-disk) virtual file returns `INVALID_FILE_ATTRIBUTES` and
the first assertion fails.

- [ ] **Step 4: Rewrite `crates/vfs-shim/src/hook.rs`**

Replace the ENTIRE contents of `crates/vfs-shim/src/hook.rs` with:

```rust
//! The ntdll detours. ALL `unsafe` in the crate lives here.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::sync::OnceLock;

use retour::RawDetour;
use vfs_redirect::{AttrDecision, Decision};
use windows_sys::Win32::Foundation::{HANDLE, HMODULE, NTSTATUS};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

use crate::engine::Engine;
use crate::ntdef::{
    FileBasicInformation, FileNetworkOpenInformation, NtCreateFileFn, NtQueryAttributesFileFn,
    NtQueryFullAttributesFileFn, ObjectAttributes, UnicodeString, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};

/// Errors installing the hooks.
#[derive(Debug)]
pub enum InstallError {
    AlreadyInstalled,
    NtdllMissing,
    ProcMissing,
    Detour,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();
// Each set once, before any detour is enabled; only read from the hooks after.
static mut TRAMP_CREATE: Option<NtCreateFileFn> = None;
static mut TRAMP_QATTR: Option<NtQueryAttributesFileFn> = None;
static mut TRAMP_QFULL: Option<NtQueryFullAttributesFileFn> = None;

/// Keeps the detours alive; dropping it disables the hooks.
pub struct HookGuard {
    _detours: Vec<RawDetour>,
}

/// Resolve `name` in ntdll and build (not yet enabled) a detour to `hookfn`.
unsafe fn make_detour(
    ntdll: HMODULE,
    name: &[u8],
    hookfn: *const (),
) -> Result<RawDetour, InstallError> {
    let proc = GetProcAddress(ntdll, name.as_ptr()).ok_or(InstallError::ProcMissing)?;
    RawDetour::new(proc as *const (), hookfn).map_err(|_| InstallError::Detour)
}

/// Install all read-path detours backed by `engine`. Idempotent-guarded.
pub fn install(engine: Engine) -> Result<HookGuard, InstallError> {
    ENGINE.set(engine).map_err(|_| InstallError::AlreadyInstalled)?;

    // SAFETY: standard ntdll lookup + detour install; each hook matches its
    // function's ABI; each trampoline is stored before any detour is enabled.
    unsafe {
        let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
        if ntdll.is_null() {
            return Err(InstallError::NtdllMissing);
        }

        let d_create = make_detour(ntdll, b"NtCreateFile\0", create_hook as *const ())?;
        TRAMP_CREATE = Some(core::mem::transmute::<*const (), NtCreateFileFn>(
            d_create.trampoline() as *const (),
        ));
        let d_qattr = make_detour(ntdll, b"NtQueryAttributesFile\0", qattr_hook as *const ())?;
        TRAMP_QATTR = Some(core::mem::transmute::<*const (), NtQueryAttributesFileFn>(
            d_qattr.trampoline() as *const (),
        ));
        let d_qfull =
            make_detour(ntdll, b"NtQueryFullAttributesFile\0", qfull_hook as *const ())?;
        TRAMP_QFULL = Some(core::mem::transmute::<*const (), NtQueryFullAttributesFileFn>(
            d_qfull.trampoline() as *const (),
        ));

        d_create.enable().map_err(|_| InstallError::Detour)?;
        d_qattr.enable().map_err(|_| InstallError::Detour)?;
        d_qfull.enable().map_err(|_| InstallError::Detour)?;

        Ok(HookGuard { _detours: vec![d_create, d_qattr, d_qfull] })
    }
}

/// Decode a fully-qualified ObjectName. `None` when ineligible (null/relative OA
/// or empty name).
unsafe fn path_of(oa: *const ObjectAttributes) -> Option<String> {
    if oa.is_null() {
        return None;
    }
    let oa_ref = &*oa;
    if !oa_ref.root_directory.is_null() || oa_ref.object_name.is_null() {
        return None;
    }
    let us = &*oa_ref.object_name;
    if us.buffer.is_null() {
        return None;
    }
    let units = core::slice::from_raw_parts(us.buffer, us.length as usize / 2);
    Some(String::from_utf16_lossy(units))
}

/// Decode + ask the engine what to do with an open.
unsafe fn decision_for(oa: *const ObjectAttributes) -> Option<Decision> {
    let engine = ENGINE.get()?;
    let path = path_of(oa)?;
    Some(engine.decide(&path))
}

unsafe extern "system" fn create_hook(
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
    let tramp = match TRAMP_CREATE {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    match decision_for(oa) {
        Some(Decision::Redirect { target_nt }) => {
            let mut wbuf: Vec<u16> = target_nt.encode_utf16().collect();
            let byte_len = (wbuf.len() * 2) as u16;
            let new_us = UnicodeString {
                length: byte_len,
                maximum_length: byte_len,
                buffer: wbuf.as_mut_ptr(),
            };
            let oa_ref = &*oa;
            let new_oa = ObjectAttributes {
                length: oa_ref.length,
                root_directory: core::ptr::null_mut(),
                object_name: &new_us,
                attributes: oa_ref.attributes,
                security_descriptor: oa_ref.security_descriptor,
                security_qos: oa_ref.security_qos,
            };
            let status = tramp(
                file_handle, access, &new_oa, iosb, alloc, attrs, share, disp, opts, ea, ealen,
            );
            drop(wbuf);
            status
        }
        Some(Decision::Deny) => STATUS_OBJECT_NAME_NOT_FOUND,
        Some(Decision::PassThrough) | None => {
            tramp(file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen)
        }
    }
}

unsafe extern "system" fn qattr_hook(
    oa: *const ObjectAttributes,
    info: *mut FileBasicInformation,
) -> NTSTATUS {
    let tramp = match TRAMP_QATTR {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if let Some(engine) = ENGINE.get() {
        if let Some(path) = path_of(oa) {
            match engine.query_attributes(&path) {
                AttrDecision::Attributes { is_dir, .. } => {
                    if !info.is_null() {
                        (*info).creation_time = 0;
                        (*info).last_access_time = 0;
                        (*info).last_write_time = 0;
                        (*info).change_time = 0;
                        (*info).file_attributes =
                            if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
                    }
                    return STATUS_SUCCESS;
                }
                AttrDecision::Deny => return STATUS_OBJECT_NAME_NOT_FOUND,
                AttrDecision::PassThrough => {}
            }
        }
    }
    tramp(oa, info)
}

unsafe extern "system" fn qfull_hook(
    oa: *const ObjectAttributes,
    info: *mut FileNetworkOpenInformation,
) -> NTSTATUS {
    let tramp = match TRAMP_QFULL {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if let Some(engine) = ENGINE.get() {
        if let Some(path) = path_of(oa) {
            match engine.query_attributes(&path) {
                AttrDecision::Attributes { is_dir, size, .. } => {
                    if !info.is_null() {
                        (*info).creation_time = 0;
                        (*info).last_access_time = 0;
                        (*info).last_write_time = 0;
                        (*info).change_time = 0;
                        (*info).allocation_size = size as i64;
                        (*info).end_of_file = size as i64;
                        (*info).file_attributes =
                            if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
                    }
                    return STATUS_SUCCESS;
                }
                AttrDecision::Deny => return STATUS_OBJECT_NAME_NOT_FOUND,
                AttrDecision::PassThrough => {}
            }
        }
    }
    tramp(oa, info)
}
```

- [ ] **Step 5: Run the attrs test + the existing hook tests**

Run: `cargo test -p vfs-shim --test hook_attrs`
Expected: PASS.

Run: `cargo test -p vfs-shim --test hook_redirect` and `--test hook_deny`
Expected: PASS (create-path behavior unchanged by the rename to `create_hook`).

- [ ] **Step 6: Run all vfs-shim tests**

Run: `cargo test -p vfs-shim`
Expected: all pass.

If the attrs test fails at runtime (wrong attribute value / size / not hidden),
STOP and report the actual values — the spike proved this exact shape works, so a
runtime failure is important signal. Do not weaken assertions.

- [ ] **Step 7: Commit**

```bash
git add crates/vfs-shim/src/hook.rs crates/vfs-shim/Cargo.toml crates/vfs-shim/tests/hook_attrs.rs
git commit -m "vfs-shim: hook NtQuery(Full)AttributesFile via query_attributes (multi-detour)"
```

---

### Task 3: Verification sweep

**Files:** none.

- [ ] **Step 1: Full workspace test**

Run: `cargo test --workspace`
Expected: all green.

- [ ] **Step 2: Warning check**

Run: `cargo build --workspace 2>&1 | rg -i "warning" || echo "no warnings"`
Expected: `no warnings`.

- [ ] **Step 3: Unsafe audit**

Confirm all `unsafe` is in `hook.rs`; `#![deny(unsafe_code)]` intact; `ntdef.rs`/`engine.rs` have no `unsafe`.

- [ ] **Step 4: Commit Cargo.lock (if changed)**

```bash
git add Cargo.lock
git commit -m "hook-attributes: update Cargo.lock" || echo "nothing to commit"
```

---

## Self-Review Notes

- **Spec coverage:** ntdef structs/consts/fn types + `query_attributes` (Task 1); multi-detour install + `path_of` + both query hooks (Task 2); every §6 assertion is in the test (virtual file, virtual dir, tombstone-hide, real pass-through, full-attr size). Verification (Task 3).
- **Derives / no-`.unwrap_err()` hazards:** the test uses `assert_eq!`/`assert_ne!` on `u32`/`u64` and `.expect()` on `install` (`HookGuard`/`InstallError` — `InstallError: Debug`; `.expect()` needs the Ok type only when it's `.expect_err`, not here — `.expect()` on `Result<HookGuard, InstallError>` needs `InstallError: Debug`, which holds).
- **Refactor safety:** `create_hook` is the old `hook` verbatim (renamed); `decision_for` now delegates to `path_of` with identical guards, so `hook_redirect`/`hook_deny` stay green.
- **Isolation:** `hook_attrs.rs` is its own single-`#[test]` binary; its global install doesn't race the other hook test binaries (separate processes).
