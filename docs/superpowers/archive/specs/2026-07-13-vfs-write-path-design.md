# VFS Write Path (Overlay + Whiteouts) — Design

**Status:** Approved (2026-07-13). Model chosen by the user: **disk overlay +
whiteouts, copy-on-write.** Builds on the completed, injection-proven read path
(7 NT detours) + DLL-loading + child-process propagation.

## Goal

Let a virtualized process create, modify, and delete files under the managed
root without touching the real game directory or the read-only mod backing
files. Writes land in a separate on-disk **overlay**; deletes leave **whiteout**
markers. The virtualized view is the layered merge of overlay, snapshot, and
real disk.

## Resolution order

For a path `P` (relative to the managed root), the effective state is the first
that matches:

1. **Overlay file** `overlay/P` exists → that file (created or COW-materialized).
2. **Whiteout** `overlay/P.__vfs_wh__` exists → `P` is deleted → hidden
   (`STATUS_OBJECT_NAME_NOT_FOUND` / omitted from listings).
3. **Snapshot** virtual file → redirect to its mod backing; snapshot tombstone →
   hidden.
4. **Real** `root/P` on disk → pass through.

This order is applied uniformly by opens, attribute queries, and directory
enumeration.

## Architecture

The overlay decisions require filesystem I/O (does `overlay/P` exist?), so they
are **impure** and live in the shim's engine layer, composed on top of the
existing **pure** snapshot transforms in `vfs-redirect` (which stay
snapshot-only). Split:

- `vfs-redirect` (pure): unchanged snapshot reasoning +
  small pure helpers for overlay path derivation and whiteout naming
  (string manipulation only, unit-testable).
- `vfs-shim` engine (impure): `Overlay { overlay_root }` performs the `exists`
  checks, COW copies, whiteout creation, and composes the final decision.

### Config change

`encode_config`/`decode_config` gain an `overlay_root` field:
`[u32 root_len][root][u32 overlay_len][overlay][snapshot]`. `Engine::new` takes
`(root, overlay_root, snapshot)`. The director creates the overlay directory;
the shim creates subdirectories on demand.

### Pure helpers (`vfs-redirect`)

- `overlay_path(overlay_root, vpath_components) -> String` — join.
- `whiteout_marker(name) -> String` — `format!("{name}.__vfs_wh__")`.
- `is_whiteout(name) -> Option<&str>` — strip the suffix, or `None`.
- Write-intent classification: `WriteIntent { write: bool, truncating: bool,
  creating: bool }` derived from `access` + `disposition` via
  `classify_open(access, disposition) -> WriteIntent`.

### Engine / Overlay (`vfs-shim`, impure)

- `Engine::decide_open(nt_path, access, disposition) -> Decision` — the new
  richer decision used by the `NtCreateFile`/`NtOpenFile` hooks:
  - Not under root → `PassThrough`.
  - **Write intent** → ensure `overlay/P` parent dirs; if opening existing
    content (not truncating/creating) and `overlay/P` absent, **materialize**:
    copy the current resolved read source (overlay else backing else real) into
    `overlay/P`; remove any stale whiteout; return `Redirect(overlay/P)`.
  - **Read intent** → overlay-first: `overlay/P` exists → `Redirect`; whiteout
    exists → `Deny`; else fall through to the existing snapshot `decide`
    (redirect backing / deny tombstone / passthrough real).
- `Engine::query_attributes` and `merge_directory` become overlay-aware:
  attributes check overlay/whiteout first; enumeration overlays the overlay
  dir's real entries on top of the merged snapshot+real listing and removes
  whiteouted names.
- `Overlay::whiteout(vpath)` — create the marker, delete the overlay copy if
  present.
- `Overlay::rename(from_vpath, to_vpath)` — materialize `from` into overlay if
  needed, move within overlay, whiteout `from`.

### Hooks (`vfs-shim/hook.rs`)

- **`NtCreateFile` / `NtOpenFile`** (extend): call `decide_open` with the
  `access` + `disposition` (`NtOpenFile` has no disposition → treat as `OPEN`).
  Track every successful under-root open as `handle -> WriteTracked { vpath,
  is_overlay }` so `NtSetInformationFile` can act by vpath.
- **`NtSetInformationFile`** (NEW detour; same ABI as `NtQueryInformationFile`):
  - class 13 `FileDispositionInformation` / class 64 `FileDispositionInformationEx`
    with the DELETE flag set, on a tracked under-root handle → `Overlay::whiteout(vpath)`;
    if the handle targets a backing/real source (not the overlay copy),
    **suppress** the real delete and return `STATUS_SUCCESS`; if it targets the
    overlay copy, let it through (and still leave the whiteout).
  - class 10 `FileRenameInformation` / class 65 `FileRenameInformationEx` → parse
    the target, `Overlay::rename`, suppress the raw rename, return success.
  - Any other class / untracked handle → pass through.
- **`NtClose`** (extend): drop the write-tracking entry too.

## Behavior matrix

| Operation | Effect |
|-----------|--------|
| Create new file under root | Written to `overlay/P`; visible to read/attrs/enum |
| Open mod (snapshot) file for write, no truncate | COW: backing copied to `overlay/P`, writes go there; backing untouched |
| Open real file for write, no truncate | COW: real copied to `overlay/P`; real untouched |
| Open for write with truncate/create (OVERWRITE/SUPERSEDE/CREATE) | `overlay/P` created empty; no copy |
| Read a file with an overlay copy | Reads the overlay copy |
| Delete a mod/real/overlay file | Whiteout marker; file hidden from read/attrs/enum; backing/real untouched |
| Read/attrs/enum of a whiteouted path | Hidden |
| Rename within root | Overlay move + source whiteout |

## Out of scope (follow-ups)

- **Rename across the root boundary** (into/out of the VFS). MVP handles rename
  within the root only.
- **Directory create/delete semantics** beyond files appearing via overlay.
- **Persistence policy / overlay GC** — the overlay simply persists on disk.
- **Concurrent writers / share-mode fidelity** beyond what the redirect gives.

## Testing strategy

- **Pure (`vfs-redirect`):** `classify_open` truth table (read vs write vs
  truncate across dispositions/access masks); `overlay_path`, `whiteout_marker`,
  `is_whiteout`.
- **Engine (`vfs-shim`):** with a temp overlay dir — `decide_open` targets
  overlay for writes and materializes; overlay-first read resolution; whiteout
  hides; `query_attributes`/`merge_directory` overlay-awareness.
- **Integration (`vfs-shim/tests`):** in-process, drive real `std::fs` — create a
  new file (appears, lands in overlay, not in root); modify a mod file (COW,
  content changes, backing unchanged); delete (whiteout hides it from read +
  `read_dir`).
- **Acceptance (cross-process, injected):** extend the exerciser with
  `write_create`, `write_modify_cow`, `write_delete` checks + a child that reads
  a file the parent created in the overlay.

## Global constraints

- Rust stable 1.97; `retour::RawDetour`; `windows-sys` 0.59.
- `vfs-redirect` stays `#![forbid(unsafe_code)]`; all `unsafe` in `hook.rs` /
  `inject.rs`.
- Fail-safe: any overlay I/O error falls back to the read-path behavior; never
  destroy a backing or real file; a failed materialize denies the write rather
  than corrupting state.

## Phasing

1. **W1 — Overlay foundation:** config `overlay_root`, pure helpers,
   overlay-aware read resolution (overlay wins, whiteout hides) in
   `decide`/`query_attributes`/`merge_directory`. Read-only overlay (no write
   hooks yet), tested by pre-populating the overlay dir.
2. **W2 — Create + COW write:** `classify_open` + `decide_open`; NtCreateFile/
   NtOpenFile write redirect + materialize; write-handle tracking.
3. **W3 — Delete → whiteout:** NtSetInformationFile disposition hook.
4. **W4 — Rename:** NtSetInformationFile rename hook.

Each phase ends green (workspace tests) and, from W2 on, extends the injected
acceptance suite.
