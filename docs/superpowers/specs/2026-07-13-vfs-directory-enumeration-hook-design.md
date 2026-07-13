# VFS Directory Enumeration Hook (Slice F) — Design

**Status:** Approved for implementation (2026-07-13). Read-path scope, "Full
dir-enum now": implement `NtQueryDirectoryFileEx` end-to-end. `NtOpenFile` and
`NtQueryInformationFile` identity spoofing follow in later slices.

## Goal

Make a virtualized directory enumerate as the *merged* view of its real
on-disk contents and the snapshot's virtual children: mod-added files appear,
mod-overrides win, tombstoned (mod-deleted) files disappear — all visible
through `std::fs::read_dir`, `FindFirstFile`, and any caller that funnels into
`ntdll!NtQueryDirectoryFileEx`.

## Background

The read path already has (Slices A–E): first-class tombstones in the snapshot,
the pure `RootMap::{decide, query_attributes, merge_directory}` transforms in
`vfs-redirect`, and live hooks on `NtCreateFile`, `NtQueryAttributesFile`, and
`NtQueryFullAttributesFile`. `merge_directory(dir_nt_path, snap, real, wildcard)
-> Vec<DirItem>` is written and exhaustively unit-tested; this slice is the hook
that *feeds* it real entries and *marshals* its result back to the caller.

A scratchpad spike (`spike-retour`) proved the whole mechanism on this Windows
build: `std::fs::read_dir` routes through `NtQueryDirectoryFileEx` with
`FileFullDirectoryInformation` (class 2), a 68-byte fixed header +
`NextEntryOffset`-chained variable-length filenames. Tracking the directory
handle at open, draining the real entries through the detour trampoline, merging
in a synthetic entry, and re-marshalling produced a correct merged listing.

## Architecture

The established split holds: **pure, exhaustively-unit-tested transforms in
`vfs-redirect`** (no `unsafe`); a **thin `unsafe` ABI translator in
`vfs-shim/hook.rs`** with one end-to-end integration test driven by real `std`.

### New pure code (`vfs-redirect`, `#![forbid(unsafe_code)]`)

Directory-info marshalling is pure byte manipulation over caller-owned slices,
so it lives with the other transforms and is tested by reading the bytes back.

- `DirInfoClass` — the `FILE_INFORMATION_CLASS` values this slice marshals:
  `Directory` (1), `FullDirectory` (2), `BothDirectory` (3), `Names` (12),
  `IdBothDirectory` (37), `IdFullDirectory` (38). A `from_u32(u32) ->
  Option<DirInfoClass>` returns `None` for anything else (fail-safe → the hook
  passes such calls straight through).
- `parse_full_dir_info(buf: &[u8]) -> Vec<DirItem>` — walk a
  `FILE_FULL_DIR_INFORMATION` (class 2) chain: `EndOfFile` @40 (i64),
  `FileAttributes` @56 (u32, `0x10` ⇒ dir), `FileNameLength` @60 (u32, bytes),
  `FileName` @68 (UTF-16). Skip `.` and `..`. Stop when `NextEntryOffset` (@0)
  is 0. Only class 2 is ever parsed — the hook always *drains the OS in class 2*
  regardless of the caller's requested class, so a multi-class parser is
  unnecessary.
- `write_dir_info(class, items, buf, single) -> DirWriteResult` — marshal
  `items` into the caller's `buf` in the requested `class`, chaining
  `NextEntryOffset`, 8-byte aligning each record, honoring the
  `SL_RETURN_SINGLE_ENTRY` (`single`) flag, and stopping when the next record
  would overflow `buf`. Returns `DirWriteResult { bytes: usize, count: usize,
  status: DirStatus }` where `DirStatus` ∈ `{Success, NoMoreFiles,
  BufferOverflow}`:
  - `count == 0 && items empty` ⇒ `NoMoreFiles`,
  - `count == 0 && items non-empty` (first record does not fit) ⇒
    `BufferOverflow`,
  - otherwise ⇒ `Success`.
  - `bytes` = end offset of the last record's data (matches the
    `IoStatusBlock.Information` the OS reports; final record is *not* tail-padded
    in the count).

  Per-class layout the writer targets (all offsets in bytes; header size =
  filename offset):

  | class | hdr/name off | FileNameLength off | attrs off | EndOfFile off | AllocationSize off |
  |------:|:------------:|:------------------:|:---------:|:-------------:|:------------------:|
  | 1 Directory      | 64  | 60 | 56 | 40 | 48 |
  | 2 FullDirectory  | 68  | 60 | 56 | 40 | 48 |
  | 3 BothDirectory  | 94  | 60 | 56 | 40 | 48 |
  | 12 Names         | 12  | 8  | —  | —  | —  |
  | 37 IdBothDir     | 104 | 60 | 56 | 40 | 48 |
  | 38 IdFullDir     | 80  | 60 | 56 | 40 | 48 |

  Only class 12 (`Names`) omits attributes/size (it carries just the name);
  every other class shares the FileNameLength@60 / attrs@56 / EndOfFile@40 /
  AllocationSize@48 layout and differs only by header size (the extra `EaSize`,
  `ShortName`, and `FileId` fields, which we zero-fill). `FileIndex` @4 and all
  unused header bytes are zeroed.

- `RootMap::contains(nt_path: &str) -> bool` — public predicate (thin wrapper
  over the existing private `under_root`), so the shim can decide at open time
  whether a handle is worth tracking without duplicating path logic.

### New engine surface (`vfs-shim/engine.rs`)

- `Engine::is_under_root(&self, nt_path: &str) -> bool` — delegates to
  `RootMap::contains`.
- `Engine::merge_directory(&self, dir_nt_path: &str, real: &[DirItem], wildcard:
  Option<&str>) -> Vec<DirItem>` — opens the snapshot reader and delegates to
  `RootMap::merge_directory`. Fail-safe: if the snapshot somehow fails to
  re-open, returns `real` unchanged (never hides real files on error).

### New hook (`vfs-shim/hook.rs`, all `unsafe`)

Three cooperating detours plus a per-handle state table:

1. **Handle table** — `static DIR_TABLE: Mutex<BTreeMap<isize, DirTracked>>`
   (`BTreeMap::new()` is `const`, unlike `HashMap`). `DirTracked { dir_nt_path:
   String, state: Option<EnumState> }`; `EnumState { merged: Vec<DirItem>,
   cursor: usize }`. Key = handle value as `isize`.

2. **`NtCreateFile` tagging (extend the existing hook)** — in the
   *PassThrough* branch only, after the trampoline returns success, if
   `engine.is_under_root(path)` insert `handle -> DirTracked { dir_nt_path:
   path, state: None }`. Redirected opens (backing file, out of root) and denied
   opens are never tagged. Tagging file handles under the root is harmless —
   `NtQueryDirectoryFileEx` is only ever issued against directory handles, and
   `NtClose` reclaims the entry regardless.

3. **`NtClose` hook (new)** — remove the handle from `DIR_TABLE` before calling
   the trampoline, so a later OS reuse of that handle value cannot inherit stale
   enumeration state.

4. **`NtQueryDirectoryFileEx` hook (new)** — for a handle *not* in the table, or
   an unsupported info class, pass straight through. Otherwise:
   - Decode `QueryFlags`: `restart = flags & SL_RESTART_SCAN`, `single = flags &
     SL_RETURN_SINGLE_ENTRY`.
   - If `state` is `None` or `restart`: extract the wildcard from the `FileName`
     `UNICODE_STRING` (treat null/empty/`*`/`*.*` as "all"); **drain the real
     directory** by calling the trampoline in a loop with class 2 +
     `SL_RESTART_SCAN` into a scratch buffer, `parse_full_dir_info` each fill,
     until `STATUS_NO_MORE_FILES`; `engine.merge_directory(dir_nt_path,
     real, wildcard)`; store `EnumState { merged, cursor: 0 }`.
   - `write_dir_info(class, &merged[cursor..], caller_buf, single)`; advance
     `cursor` by `count`; write `IoStatusBlock` (`Status` @0 as u32,
     `Information` @8 as `usize` = `bytes`); return the mapped `NTSTATUS`.

   Draining with class 2 regardless of the caller's class means the OS-side
   parse is single-class while the caller still receives whatever class it asked
   for. The detour trampoline bypasses our own hook, so draining does not
   recurse.

## Data flow (a `read_dir` over a merged directory)

```
CreateFileW(dir)                    read_dir loop: NtQueryDirectoryFileEx(h, ..., class 2)
   -> NtCreateFile hook                -> hook: h in DIR_TABLE, class supported
        passthrough opens real dir          state None -> drain real via trampoline (class 2)
        under root -> DIR_TABLE[h]=path                   parse_full_dir_info -> real: Vec<DirItem>
                                                          engine.merge_directory(path, real, wc)
                                                          store merged, cursor=0
                                              write_dir_info(class2, merged[0..], buf, single)
                                              set IoStatusBlock; return SUCCESS
                                          ... subsequent calls serve from cursor ...
                                          cursor == len -> write 0 entries -> NoMoreFiles
CloseHandle(dir) -> NtClose hook -> DIR_TABLE.remove(h)
```

## Behavior matrix (what this slice covers)

| Case | Result |
|------|--------|
| Real file under a merged dir, no snapshot entry | Passes through (appears) |
| Mod-added virtual file | Appears (from snapshot) |
| Mod override of a real file | Appears once, snapshot's size/attrs win |
| Tombstoned real file | Omitted from the listing |
| Virtual sub-directory | Appears as a `DIRECTORY` entry in the parent |
| Wildcard (`*.esp`, etc.) on first call | Filters the merged names |
| `SL_RESTART_SCAN` | Rebuilds the merged view, resets the cursor |
| `SL_RETURN_SINGLE_ENTRY` | Emits exactly one entry per call |
| Buffer too small for the first entry | `STATUS_BUFFER_OVERFLOW` |
| Cursor exhausted | `STATUS_NO_MORE_FILES` |
| Handle not tracked / out of root | Passes through untouched |
| Unsupported info class | Passes through untouched |

## Out of scope (explicit non-goals for this slice)

- **Pure-virtual-directory *enumeration*.** A mod-added directory that has no
  real on-disk counterpart *appears* in its parent's listing (via
  `merge_directory`), but *opening* it to enumerate its own contents still fails
  at `NtCreateFile` (the path is not on disk). Synthesizing an openable handle
  for a non-existent directory is a distinct concern (it interacts with
  create-disposition handling) and gets its own follow-up slice.
- **`NtQueryDirectoryFile` (the non-Ex form).** The agreed sequencing is
  `NtQueryDirectoryFileEx` end-to-end first. The non-Ex entry point (older
  `FindFirstFile` paths) is a later slice; the pure marshaller built here is
  reused verbatim when it lands.
- **Write path** (rename/delete/create/materialize) — deferred to P3.

## Testing strategy

- **Pure (`vfs-redirect`):** `write_dir_info` unit-tested per class (2, 3, 12 at
  minimum; 1/37/38 header sizes verified) — read offsets back, verify the
  `NextEntryOffset` chain, 8-byte alignment, names, sizes, attrs; verify
  `single`, `BufferOverflow` (buffer too small for first record), and
  `NoMoreFiles` (empty input). `parse_full_dir_info` tested by round-tripping a
  class-2 buffer produced by `write_dir_info` (including `.`/`..` skip and a
  directory-attribute entry). `RootMap::contains` in/out of root.
- **Engine (`vfs-shim`):** `is_under_root` and `merge_directory` delegate
  correctly (in-root merges, out-of-root returns filtered real).
- **Integration (`vfs-shim/tests/hook_direnum.rs`):** build a real directory
  with real files, a real subdir, and a to-be-tombstoned file; a snapshot that
  adds a virtual file, overrides a real file's size, tombstones the deleted
  file, and adds a virtual directory; `install`; then `std::fs::read_dir` and
  assert the merged, deduped, tombstone-hidden, override-winning listing —
  exercising real-drain, merge, class-2 marshalling, multi-call cursoring, and
  `IoStatusBlock` correctness end-to-end through `std`.

## Global constraints

- Rust stable (1.97); `retour::RawDetour` only (no `static_detour!`).
  `windows-sys` 0.59 (`HANDLE`/`HMODULE` are `*mut c_void`).
- `vfs-redirect` stays `#![forbid(unsafe_code)]`; all `unsafe` remains confined
  to `vfs-shim/hook.rs`.
- Fail-safe everywhere: any decode failure, unknown class, untracked handle, or
  snapshot error results in unmodified pass-through — the VFS must never make a
  real directory *less* visible than it is without a hook.
