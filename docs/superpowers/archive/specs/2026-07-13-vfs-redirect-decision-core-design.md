# vfs-redirect Decision Core — Design Spec

**Status:** Approved-to-proceed (standing goal: drive to a working end-to-end
VFS), ready for planning.
**Date:** 2026-07-13
**Slice:** Sixth slice / shim sub-slice 5a — the **pure redirect-decision core**
of the injected shim. Given an incoming NT open path and a published snapshot, it
decides *pass the open through unchanged* vs *reissue the open against a mod
backing file*. No hooks, no injection, no `windows-sys` — pure, in-process
testable logic that de-risks the shim's brain before any OS interception.
**Parent docs:** *VFS Design* (constraints C1–C3 "control the bytes, not the
mapping"; redirect-to-backing-file), *IPC Architecture* (the snapshot the shim
reads).
**Depends on:** `vfs-core` (`normalize_vpath`, `fold`, `PathError`), `vfs-shared`
(`SnapshotReader`, `SnapResolution`).

---

## 1. Context & positioning

The injected shim hooks the NT file-open routines (`NtCreateFile`,
`NtOpenFile`). Each call carries a target path as a UTF-16 `UNICODE_STRING` (e.g.
`\??\C:\Games\Skyrim\Data\foo.esp`). The shim must, with **zero heap surprises
and no recursion**, answer one question per open: *does this path resolve to a
virtualized (mod) file, and if so, what real backing file should the kernel open
instead?* Everything else about the shim — installing the detours, marshalling
`OBJECT_ATTRIBUTES`, calling the original routine, injection — is mechanism
around that one decision.

This slice isolates the decision as a **pure function over a `&str` path + a
`SnapshotReader`**, so it is unit-testable on any host with no Windows APIs, no
`unsafe`, and no live process. It mirrors the project's established split: pure
logic crates are `#![forbid(unsafe_code)]`; the OS/`unsafe` layer (the DLL,
next slice) wraps them.

### Why a redirect (not a fake handle)

Per C1–C3, images are never served via `ReadFile` and section identity is fixed
by the kernel; the robust MVP is to **redirect the open to the real mod file on
disk** and let the kernel do its normal thing. Reads — the 90% modding case —
work immediately because `vfs-core`/the snapshot already carry each virtual
file's real backing `source` path. Identity spoofing (making the returned handle
report the *virtual* path back to the app) is deferred; most reads don't need it.

---

## 2. Scope & crate boundary

New crate `crates/vfs-redirect`, stable Rust, `#![forbid(unsafe_code)]`, pure.

### In scope

- `RootMap` — holds the managed install root (the VFS mount point) as normalized
  path components; `new(root)` accepts either NT (`\??\C:\Games\Skyrim`) or Win32
  (`C:\Games\Skyrim`) form.
- `Decision` — `PassThrough` | `Redirect { target_nt: String }`.
- `RootMap::decide(&self, nt_path: &str, snap: &SnapshotReader) -> Decision` — the
  core: match the path under the root (case-insensitively, component-wise),
  normalize the remainder to a vpath, fold each component, `snap.resolve(..)`, and
  map the result to a decision.
- `utf16_to_string(&[u16]) -> String` and `string_to_utf16(&str) -> Vec<u16>` —
  thin, pure conversion helpers the hook layer will use to bridge
  `UNICODE_STRING` ⇄ Rust (no trailing NUL; `UNICODE_STRING` is length-counted).
- One tiny public-API addition to `vfs-core`: `pub use casefold::fold;` — the
  `SnapshotReader::resolve` contract requires **pre-folded** components, so any
  external querier needs the same `fold`. It belongs beside `normalize_vpath`.

### Explicitly out of scope (later slices)

- The actual `retour` detours on `NtCreateFile`/`NtOpenFile` (slice 5b).
- `OBJECT_ATTRIBUTES` / `RootDirectory`-relative opens (a handle-relative open
  whose `RootDirectory` is non-null); MVP handles only fully-qualified
  `ObjectName` paths and passes the rest through.
- `\Device\HarddiskVolumeN\...` volume-form paths (MVP matches the `\??\`
  DOS-drive form and bare Win32 form; volume form → pass through).
- Directory enumeration virtualization (a `Dir` result → pass through for now).
- Tombstone/deny semantics (a deleted virtual file); MVP has no tombstones live.
- Injection, the DLL, identity spoofing, write redirection/materialize.

---

## 3. The decision algorithm

`decide(nt_path, snap)`:

1. **Normalize the incoming path.** `normalize_vpath(nt_path)` strips a leading
   `\??\` / `\\?\` prefix, unifies `\`/`/`, and resolves `.`/`..` into a
   `/`-joined form that *includes the drive component*, e.g.
   `\??\C:\Games\Skyrim\Data\foo.esp` → `C:/Games/Skyrim/Data/foo.esp`. On
   `PathError` (e.g. `..` escaping) → `PassThrough` (can't be a valid virtual
   file; let the real open handle it).
2. **Component-wise root match.** Split both the normalized path and the stored
   root into `/`-components. If the path has fewer components than the root, or
   any of the first `root.len()` components differ under `fold`, → `PassThrough`.
3. **Relative remainder → resolution.** Take the components *after* the root
   (e.g. `["Data","foo.esp"]`), `fold` each, and call
   `snap.resolve(&folded_refs)`:
   - `File { source, .. }` → `Redirect { target_nt: render_nt(source) }`.
   - `Dir` → `PassThrough` (directory virtualization deferred; a real dir open
     proceeds).
   - `NotFound` → `PassThrough` (either a genuine game file not overridden by any
     mod, or truly absent — the real open is correct either way).

`render_nt(source: &[u8])`: interpret `source` as UTF-8 (`from_utf8_lossy`; the
director stores backing paths as UTF-8 absolute Win32 paths, e.g.
`D:\Mods\Cool\foo.esp`) and prepend the NT DOS-device prefix:
`format!(r"\??\{}", s)`. **Contract:** `source` is an absolute Win32 path with a
drive letter and no NT prefix; the director guarantees this. (A `source` that
already begins with `\??\` or `\\?\` is passed through unchanged rather than
double-prefixed — a cheap guard.)

### Worked example

Root `\??\C:\Games\Skyrim`; snapshot has `data/foo.esp` →
source `D:\Mods\Cool\foo.esp`.
Open of `\??\C:\Games\Skyrim\Data\foo.esp`
→ normalized `C:/Games/Skyrim/Data/foo.esp`
→ root matches (`c:/games/skyrim`), remainder `["Data","foo.esp"]`
→ folded `["data","foo.esp"]` → `resolve` → `File{source:"D:\\Mods\\Cool\\foo.esp"}`
→ `Redirect { target_nt: "\\??\\D:\\Mods\\Cool\\foo.esp" }`.

---

## 4. API

```rust
pub enum Decision {
    /// Let the original NT open proceed unchanged.
    PassThrough,
    /// Reissue the open against this NT path (the mod backing file).
    Redirect { target_nt: String },
}

pub struct RootMap { /* normalized root components (original case) */ }

impl RootMap {
    /// `root` may be NT (`\??\C:\Games\Skyrim`) or Win32 (`C:\Games\Skyrim`).
    pub fn new(root: &str) -> Result<Self, vfs_core::PathError>;
    pub fn decide(&self, nt_path: &str, snap: &vfs_shared::SnapshotReader) -> Decision;
}

/// UNICODE_STRING (length-counted, no NUL) ⇄ Rust helpers for the hook layer.
pub fn utf16_to_string(units: &[u16]) -> String;   // String::from_utf16_lossy
pub fn string_to_utf16(s: &str) -> Vec<u16>;       // s.encode_utf16().collect(), no NUL
```

`Decision` derives `Debug, Clone, PartialEq, Eq` (asserted in tests).

---

## 5. Error handling

No panics. The only fallible entry is `RootMap::new` (returns `PathError` if the
root itself normalizes to an escaping path — practically never). `decide` is
total: every path maps to `PassThrough` or `Redirect`, treating any malformed or
out-of-root input as `PassThrough` (fail safe — never redirect what you can't
positively resolve).

---

## 6. Testing (all pure, host-runnable)

Build a small snapshot with `vfs_shared::bridge::flatten(&VfsTree)` (the existing
bridge) from a couple of `vfs-core` layers, open it with `SnapshotReader`, then:

- **Redirect hit:** open under root that resolves to a file →
  `Redirect { target_nt }` with the exact `\??\`-prefixed source.
- **Case-insensitive root + path:** `\??\c:\games\SKYRIM\DATA\Foo.ESP` still
  redirects (fold on both root and remainder).
- **Pass-through, outside root:** `\??\C:\Windows\System32\kernel32.dll` →
  `PassThrough`.
- **Pass-through, under root but not virtualized:** a path the snapshot doesn't
  contain → `PassThrough`.
- **Pass-through, directory:** open of a virtual directory → `PassThrough`.
- **Escaping path:** `\??\C:\Games\Skyrim\..\..\..\evil` → `PassThrough` (no
  panic).
- **Win32-form root** in `new` (`C:\Games\Skyrim`) matches an NT-form open.
- **`source` already NT-prefixed** → not double-prefixed.
- **UTF-16 helpers:** round-trip `string_to_utf16`→`utf16_to_string`; lossy
  handling of an unpaired surrogate doesn't panic.

---

## 7. Dependencies & toolchain

- **Toolchain:** stable.
- **Dependencies:** `vfs-core` (path), `vfs-shared` (path; needs its default
  reader API — no `bridge` feature required by the crate itself, but tests use
  `vfs-shared`'s `bridge` feature + `vfs-core` to build fixtures, so the
  dev-dependency enables `features = ["bridge"]`).
- **Unsafe:** none (`#![forbid(unsafe_code)]`).
- **vfs-core change:** add `pub use casefold::fold;` (one line).
- **Workspace:** add `crates/vfs-redirect` to `members`.

---

## 8. Out-of-scope reminders

No detours, no injection, no `OBJECT_ATTRIBUTES`, no `RootDirectory`-relative or
`\Device\` paths, no directory virtualization, no writes, no identity spoofing.

*End of spec.*
