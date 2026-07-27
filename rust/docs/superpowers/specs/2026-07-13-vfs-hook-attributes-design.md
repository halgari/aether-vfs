# Hooks: Path-Based Attribute Queries — Design Spec

**Status:** Approved-to-proceed (read-path hook program, Slice E), ready for planning. **De-risked by a passing spike.**
**Date:** 2026-07-13
**Slice:** **Slice E** — hook `NtQueryAttributesFile` and
`NtQueryFullAttributesFile` so path-based attribute checks (`GetFileAttributesW`,
`GetFileAttributesExW`) see virtual files/dirs, and tombstoned files appear
absent. Introduces the multi-detour install infrastructure.
**Parent docs:** hook-surface plan + hook recipe (memory).
**Depends on:** Slice B (`query_attributes`/`AttrDecision`), Slice D (hook shape).

---

## 1. Context

`GetFileAttributesW` (existence/attr checks, extremely common) routes to
`NtQueryAttributesFile`; `GetFileAttributesExW` to `NtQueryFullAttributesFile`
(also returns size). Neither is `NtCreateFile`, so a virtual-only file currently
reports "not found" to these APIs even though it should exist. (Note: `std::fs::
metadata` opens+queries a handle, so it already works via the `NtCreateFile`
redirect — these hooks cover the *path-based* query APIs.) This slice answers both
from the snapshot via the Slice-B `query_attributes` transform, and adds the
multi-detour install machinery the remaining hooks will share.

Spike-validated: both detours install and fire correctly; the `#[repr(C)]` info
structs fill correctly; `GetFileAttributesW` returns `INVALID_FILE_ATTRIBUTES`
when the hook returns `STATUS_OBJECT_NAME_NOT_FOUND`.

---

## 2. Scope & boundary

Only `crates/vfs-shim`. `unsafe` confined to `hook.rs`.

### In scope

- `ntdef`: `#[repr(C)]` `FileBasicInformation` (40B) and
  `FileNetworkOpenInformation` (56B); `NtQueryAttributesFileFn`/
  `NtQueryFullAttributesFileFn` types; consts `STATUS_SUCCESS = 0`,
  `FILE_ATTRIBUTE_DIRECTORY = 0x10`, `FILE_ATTRIBUTE_NORMAL = 0x80`.
- `engine`: `Engine::query_attributes(nt_path) -> vfs_redirect::AttrDecision`
  (opens a `SnapshotReader` per call; fail-safe `PassThrough`).
- `hook`: multi-detour install (a `make_detour` helper; `HookGuard` holds
  `Vec<RawDetour>`; per-function trampoline statics); extract `path_of(oa)` shared
  by all path hooks; the two query hooks fill the info struct / return
  `STATUS_OBJECT_NAME_NOT_FOUND` for tombstones / pass through otherwise.
- Integration test via `GetFileAttributesW`/`GetFileAttributesExW`.

### Out of scope

- `NtQueryDirectoryFile` (Slice F), `NtOpenFile` / identity (later), writes.
- Accurate timestamps (times set to 0 for MVP — `GetFileAttributesW` reads only
  the attribute flags; `Ex` additionally reads size, which we fill).
- `RootDirectory`-relative queries (`path_of` returns `None` → pass through).

---

## 3. Behavior

Each query hook, for a decoded `path_of(oa)`:
- `AttrDecision::Attributes { is_dir, size, .. }` → fill the caller's info struct
  (`FileAttributes = DIRECTORY|NORMAL`; times 0; `NtQueryFullAttributesFile` also
  sets `eof`/`alloc_size = size`) and return `STATUS_SUCCESS`.
- `AttrDecision::Deny` → return `STATUS_OBJECT_NAME_NOT_FOUND` (hidden).
- `AttrDecision::PassThrough` (or `path_of`/engine `None`) → call the trampoline.

`install` now sets up three detours (NtCreateFile + the two query fns), storing a
trampoline per function; `HookGuard` owns all three `RawDetour`s.

---

## 4. API

No public API change beyond `Engine::query_attributes`. `ntdef` gains structs +
consts + fn types; `hook.rs` gains two hooks + `make_detour`/`path_of` helpers;
`HookGuard` internally holds `Vec<RawDetour>` (field private; unchanged surface).

## 5. Error handling

Query hooks never panic; a null info pointer is tolerated (skip the fill, still
return the status). Trampoline-unset invariant returns `STATUS_UNSUCCESSFUL`.

## 6. Testing

Single-test integration binary `tests/hook_attrs.rs` (its own process). Temp root
with a real `real.esp` (not in snapshot) and a real `gone.esp` (tombstoned).
Snapshot: virtual file `mod.esp` (size 1234, backing path), virtual dir `moddir`,
tombstone `gone.esp`. Install, then via `windows-sys`:
- `GetFileAttributesW(root/mod.esp)` (absent on disk) ≠ `INVALID_FILE_ATTRIBUTES`,
  and the `DIRECTORY` bit is clear.
- `GetFileAttributesW(root/moddir)` (absent on disk) has the `DIRECTORY` bit set.
- `GetFileAttributesW(root/gone.esp)` (present on disk) == `INVALID_FILE_ATTRIBUTES`
  (tombstone hides it).
- `GetFileAttributesW(root/real.esp)` (present, not virtual) ≠ INVALID (pass
  through).
- `GetFileAttributesExW(root/mod.esp)` → success and reported size == 1234.

## 7. Dependencies & toolchain

Stable. `unsafe` confined to `hook.rs`. Add a **dev-dependency** on `windows-sys`
with `["Win32_Foundation", "Win32_Storage_FileSystem"]` for the test's
`GetFileAttributes*` calls (feature-unifies with the normal dep in test builds).

## 8. Out-of-scope reminders

No dir enumeration, no NtOpenFile/identity, no writes, no relative queries, no
timestamp fidelity.

*End of spec.*
