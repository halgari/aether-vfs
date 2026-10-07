# VFS Handle Identity (Slice G) — Design

**Status:** Approved for implementation (2026-07-13). Read-path scope. Completes
the read path's identity concern: a redirected virtual file, when asked "what is
your path?", reports the *virtual* path, not the backing file.

## Goal

After a virtual file open is redirected to a backing file (Slices D/F already do
this via `NtCreateFile`/`NtOpenFile`), handle-based path queries — chiefly
`GetFinalPathNameByHandleW` — must return the virtual path the caller opened,
preserving the illusion that the mod file lives where the game expects it.

## Background & spike findings

Redirection hands the caller a handle to the *backing* file, so any handle-based
identity query naturally reports the backing path. A scratchpad spike established
exactly which `NtQueryInformationFile` info classes the relevant APIs use:

- `std::fs::File::open` → class **18** (`FileAllInformation`) for size/times.
  **Not touched** — size is already correct through the redirect, and rewriting
  the embedded name risks breaking `std::fs::metadata`.
- `GetFinalPathNameByHandleW` → class **9** (`FileNameInformation`) then class
  **48** (`FileNormalizedNameInformation`).
- **Spoofing only class 48 works**: `GetFinalPathNameByHandleW` then returns
  `\\?\<drive><virtual-path>` while reads still hit the backing bytes.
- **Spoofing class 9 breaks it**: `GetFinalPathNameByHandleW` re-resolves the
  class-9 name against the real volume, the non-existent virtual path fails, and
  the call returns empty. So class 9 must pass through.

## Architecture

Same split as the rest of the read path: pure transforms in `vfs-redirect`, a
thin `unsafe` detour in `vfs-shim/hook.rs`.

### Pure code (`vfs-redirect`, `#![forbid(unsafe_code)]`)

- `nt_to_volume_relative(nt_path: &str) -> String` — strip a `\??\` or `\\?\`
  prefix, then drop a leading `X:` drive, yielding the volume-relative path
  (leading `\`, no drive) that `FILE_NAME_INFORMATION` carries. Idempotent on an
  already-relative path.
- `write_file_name_info(name: &str, buf: &mut [u8]) -> NameWriteResult` — marshal
  a `FILE_NAME_INFORMATION` / `FILE_NORMALIZED_NAME_INFORMATION` record:
  `FileNameLength` (u32, bytes) @0, UTF-16LE `FileName` (no NUL) @4. Returns
  `NameWriteResult { bytes: usize, status: DirStatus }` (reusing `DirStatus`):
  `BufferOverflow` when `buf.len() < 4 + namelen` (writes only `FileNameLength`,
  the documented behavior), else `Success` with `bytes = 4 + namelen`.

### Engine surface (`vfs-shim/engine.rs`)

No new engine method needed — the virtual path is known at hook time (it is the
`ObjectAttributes` name the caller passed before redirection). The pure helpers
above are called directly from the hook.

### Hook (`vfs-shim/hook.rs`, all `unsafe`)

- **`IDENTITY_TABLE: Mutex<BTreeMap<isize, String>>`** — handle → virtual
  volume-relative path. Separate from `DIR_TABLE` (which tracks *directory*
  handles for enumeration); this tracks *redirected file* handles.
- **Capture at redirect.** In the `Decision::Redirect` branch of both
  `create_hook` and `open_hook`, after the redirected open returns success,
  insert `*file_handle -> nt_to_volume_relative(original_virtual_path)`. The
  original path is `path_of(oa)` computed before rewriting `ObjectAttributes`.
- **`NtQueryInformationFile` hook (new).** For a tracked handle and class 48
  (`FileNormalizedNameInformation`), marshal the stored virtual path via
  `write_file_name_info`, set `IoStatusBlock` (`Status` @0, `Information` @8 =
  `bytes`), and return the mapped status. Everything else — untracked handle,
  any other class (including 9 and 18) — passes straight through.
- **`NtClose` cleanup.** Extend `close_hook` to also remove the handle from
  `IDENTITY_TABLE`.

## Data flow

```
File::open(virtual)  -> NtCreateFile/NtOpenFile hook: Decision::Redirect
                          rewrite OA to backing, open, success
                          IDENTITY_TABLE[handle] = volrel(virtual path)
GetFinalPathNameByHandleW(handle)
   -> NtQueryInformationFile(handle, class 9)  -> passthrough (backing name)
   -> NtQueryInformationFile(handle, class 48) -> hook: tracked -> write virtual
   -> returns \\?\C:<virtual path>
CloseHandle(handle) -> NtClose hook -> DIR_TABLE.remove + IDENTITY_TABLE.remove
```

## Behavior matrix

| Case | Result |
|------|--------|
| Redirected virtual file, class 48 query | Virtual path returned |
| Redirected virtual file, class 9 query | Backing name (passes through) — required for `GetFinalPathNameByHandleW` |
| Redirected virtual file, class 18 (`FileAllInformation`) | Passes through (size correct from backing) |
| Non-redirected (real) handle | Passes through untouched |
| Buffer too small for the name | `STATUS_BUFFER_OVERFLOW`, `FileNameLength` still written |

## Out of scope / non-goals

- **Cross-volume identity.** `GetFinalPathNameByHandleW` prepends the *backing*
  volume's drive to the class-48 name, so identity is faithful only when backing
  and virtual live on the same drive (the normal modding case). Documented, not
  fixed.
- **Class 9 / `FileAllInformation` name spoofing.** Left as pass-through by
  design (class 9 would break `GetFinalPathNameByHandleW`; class 18 is
  unnecessary and risky).

## Testing strategy

- **Pure (`vfs-redirect`):** `nt_to_volume_relative` over `\??\C:\..`, `\\?\C:\..`,
  and already-relative inputs; `write_file_name_info` success (read back
  `FileNameLength` + name) and buffer-overflow.
- **Integration (`vfs-shim/tests/hook_identity.rs`):** snapshot with a virtual
  file backed by a real on-disk blob with a *distinct* name; open the virtual
  path (redirects, content readable), then `GetFinalPathNameByHandleW` and assert
  the returned path contains the virtual filename and not the backing filename.

## Global constraints

- Rust stable 1.97; `retour::RawDetour`; `windows-sys` 0.59.
- `vfs-redirect` stays `#![forbid(unsafe_code)]`; all `unsafe` in `hook.rs`.
- Fail-safe: any decode failure, untracked handle, or non-48 class → unmodified
  pass-through.
