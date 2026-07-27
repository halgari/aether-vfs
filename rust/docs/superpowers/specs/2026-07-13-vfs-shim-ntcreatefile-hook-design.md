# vfs-shim NtCreateFile Hook — Design Spec

**Status:** Approved-to-proceed (standing goal: drive to a working end-to-end
VFS), ready for planning. **De-risked by a passing in-process spike.**
**Date:** 2026-07-13
**Slice:** Seventh slice / shim sub-slice 5b — the **actual interception**: install
a `retour` detour on `NtCreateFile`, and on each open consult the redirect
decision core + a snapshot to reroute virtualized opens to their mod backing
files. Proven in-process (no injection yet) by opening a non-existent virtual
path with `std::fs` and reading the backing file's bytes.
**Parent docs:** *VFS Design* (C1–C3), *IPC Architecture*.
**Depends on:** `vfs-redirect` (`RootMap`, `Decision`, UTF-16 helpers),
`vfs-shared` (`SnapshotReader`, `LayoutError`), `vfs-core` (`PathError`),
`retour` 0.3, `windows-sys` 0.59.
**Validated recipe:** see the memory note *vfs-ntcreatefile-hook-recipe* — the
spike confirmed every FFI detail below on stable Rust 1.97.

---

## 1. Context & positioning

`vfs-redirect` decides *what* to do with a path; this slice does it *at the OS
boundary*. A `retour::RawDetour` patches `ntdll!NtCreateFile` so that every file
open in the process passes through our hook. The hook reads the target path from
`OBJECT_ATTRIBUTES.ObjectName`, asks the engine (`RootMap` + snapshot) for a
`Decision`, and — on `Redirect` — reissues the open against the mod backing
file's NT path via the detour trampoline; otherwise it calls the trampoline
unchanged. Reads of virtualized files thus transparently come from mod dirs with
no on-disk footprint in the game folder.

This is M1, the project's highest-risk milestone. A standalone spike already
proved the mechanism end-to-end in-process; this slice packages it as a real
crate wired to `vfs-redirect` + a real `SnapshotReader`, with the same in-process
proof as an automated test. **Injection into another process is the next slice
(5c); here everything runs in the test's own process.**

### Why NtCreateFile (not CreateFileW)

Per C1–C3, interception must be at the NT layer to catch opens that bypass the
Win32 layer and to remain agnostic to how the game opens files. Conveniently,
`CreateFileW` (and thus `std::fs`) funnels into `NtCreateFile` with the target in
`\??\C:\...` NT DOS-device form — exactly what `vfs-redirect::render_nt`
produces — so the layers already agree on path form.

---

## 2. Scope & crate boundary

New crate `crates/vfs-shim`, stable Rust, `#![deny(unsafe_code)]` with all
`unsafe` confined to the hook module (the pattern used by `vfs-ipc`/`vfs-win`).

### In scope

- **`ntdef`** (pure type defs, no `unsafe`): `#[repr(C)]` `UnicodeString`
  (`length`/`maximum_length` in **bytes**, `buffer: *mut u16`) and
  `ObjectAttributes` (`length`, `root_directory: HANDLE`, `object_name`,
  `attributes`, two `*const c_void`); the `NtCreateFileFn` `unsafe extern
  "system"` signature; `NTSTATUS` constants used (`STATUS_UNSUCCESSFUL`).
- **`engine::Engine`** (no `unsafe`): owns a `RootMap` + the snapshot bytes
  (`Vec<u8>`); `Engine::new(root, snapshot)` validates both; `Engine::decide(&self,
  nt_path: &str) -> Decision` opens a `SnapshotReader` over its bytes per call
  (cheap, self-reference-free) and delegates to `RootMap::decide`.
- **`hook`** (all `unsafe` here): the `extern "system"` hook fn; `install(engine)
  -> Result<HookGuard, InstallError>` which resolves `ntdll!NtCreateFile`
  (`GetModuleHandleA`/`GetProcAddress`), builds+enables a `RawDetour`, stashes the
  trampoline; `HookGuard` (owns the `RawDetour`; `Drop` disables the hook). A
  process-global `OnceLock<Engine>` + `static mut Option<NtCreateFileFn>`
  trampoline.
- An in-process integration test proving a `std::fs::read_to_string` of a
  non-existent virtual path returns the backing file's bytes.

### Explicitly out of scope (later slices)

- **Injection** into another process, the DLL entry point / `DllMain`, sharing
  the snapshot section cross-process (5c) — here the engine owns an in-memory
  `Vec<u8>` snapshot.
- Hooking `NtOpenFile` (same pattern; trivial follow-up once 5b lands).
- `RootDirectory`-relative opens (handle-relative) — hook passes them through.
- Directory enumeration (`NtQueryDirectoryFile[Ex]`), writes/materialize,
  identity spoofing, live snapshot swap via seqlock, x86/WOW64.
- A live shared-memory `SnapshotReader` that re-reads under the seqlock — this
  slice takes a static owned snapshot; the seqlock reader is wired in 5c.

---

## 3. The hook algorithm (validated by the spike)

`hook(file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen)`:

1. Load the trampoline; if somehow unset, return `STATUS_UNSUCCESSFUL` (invariant:
   set before `enable`, so this never triggers — but never panic across the FFI
   boundary).
2. If the engine is set and `!oa.is_null()`, read `*oa`. Act only when
   `root_directory.is_null()` (fully-qualified open) and `object_name` +
   `object_name.buffer` are non-null. Otherwise fall through to a plain
   trampoline call.
3. Decode the path: `from_raw_parts(buffer, length/2)` → `from_utf16_lossy`.
4. `engine.decide(&path)`:
   - `Redirect { target_nt }`: build a local `Vec<u16>` of `target_nt`, a local
     `UnicodeString` over it, and a copy of `*oa` with `object_name` pointing at
     the new `UnicodeString`; call the trampoline with `&new_oa`. The locals
     outlive the synchronous call. No recursion — the trampoline bypasses the
     patch, and backing paths live outside the root.
   - `PassThrough`: call the trampoline with the original `oa`.
5. Any missing precondition ⇒ plain trampoline call with the original arguments.

**No panics, no hookable I/O inside the hook** (only allocation + pure logic),
per the reentrancy caution in the recipe memory.

`install(engine)`:
1. `ENGINE.set(engine)` (error `AlreadyInstalled` if already set).
2. `GetModuleHandleA("ntdll.dll\0")`; `GetProcAddress(_, "NtCreateFile\0")`.
3. `RawDetour::new(addr, hook)`, stash `TRAMPOLINE = transmute(detour.trampoline())`,
   `detour.enable()`.
4. Return `HookGuard { detour }`. Production keeps it alive for the process
   lifetime (`std::mem::forget` or a static); the test holds it in a local so the
   hook is torn down on drop.

---

## 4. API

```rust
// engine.rs
pub struct Engine { /* map: RootMap, snapshot: Vec<u8> */ }
#[derive(Debug)]
pub enum EngineError { Root(vfs_core::PathError), Snapshot(vfs_shared::LayoutError) }
impl Engine {
    pub fn new(root: &str, snapshot: Vec<u8>) -> Result<Self, EngineError>;
    pub fn decide(&self, nt_path: &str) -> vfs_redirect::Decision;
}

// hook.rs
#[derive(Debug)]
pub enum InstallError { AlreadyInstalled, NtdllMissing, ProcMissing, Detour }
pub struct HookGuard { /* detour: RawDetour */ }   // Drop disables the hook
pub fn install(engine: Engine) -> Result<HookGuard, InstallError>;
```

`lib.rs` re-exports `Engine`, `EngineError`, `InstallError`, `HookGuard`,
`install`.

---

## 5. Error handling

- `Engine::new` validates the root (`RootMap::new`) and the snapshot
  (`SnapshotReader::open`) up front, surfacing `EngineError`.
- `install` returns `InstallError` for every failure (never panics).
- The hook never panics and never returns spuriously: unmatched opens are
  byte-for-byte identical trampoline calls.
- `Engine::decide` treats a snapshot that fails to re-open as `PassThrough`
  (fail-safe; can't happen after `new` validated it, but defensive).

## 6. Concurrency / testing constraints

Installing the detour mutates process-global code. The hook integration test
lives in its **own** test binary (`tests/hook_redirect.rs`) with a **single**
`#[test]`, so no other test in the process races the global hook. `Engine`'s own
logic (validation, `decide`) is unit-tested separately without touching the
global (no hook install), so those can run in parallel safely.

---

## 7. Testing

- **Engine unit tests** (no hook): `Engine::new` rejects a bad snapshot
  (`EngineError::Snapshot`); `decide` returns `Redirect`/`PassThrough` for the
  same fixtures `vfs-redirect` uses (sanity that the wiring matches).
- **Hook integration test** (`tests/hook_redirect.rs`, single test): create a
  temp dir as the root; write a real backing file elsewhere (or in a `backing`
  subdir); build a snapshot mapping `virtual.txt` → the backing file's absolute
  Win32 path (via `vfs-core` + `vfs-shared` `bridge::flatten`); assert the
  virtual path does NOT exist on disk; `install(Engine::new(root, snapshot))`;
  `std::fs::read_to_string(root/virtual.txt)` returns the backing bytes; drop the
  guard. Mirrors the validated spike.

---

## 8. Dependencies & toolchain

- **Toolchain:** stable (spike-confirmed; `RawDetour` needs no nightly).
- **Dependencies:** `vfs-redirect` (path), `vfs-shared` (path), `vfs-core` (path),
  `retour = { version = "0.3", default-features = false }`, `windows-sys =
  { version = "0.59", features = ["Win32_Foundation", "Win32_System_LibraryLoader"] }`.
  Dev-dep: `vfs-shared` with `features = ["bridge"]` for the snapshot fixtures.
- **Unsafe:** `#![deny(unsafe_code)]` with localized `#[allow(unsafe_code)]` in
  `hook.rs` only; `ntdef.rs`/`engine.rs` are `unsafe`-free.
- **Workspace:** add `crates/vfs-shim` to `members`.

---

## 9. Out-of-scope reminders

No injection/DLL, no NtOpenFile, no RootDirectory-relative or directory ops, no
writes, no shared-memory/seqlock reader, no identity spoofing, no WOW64.

*End of spec.*
