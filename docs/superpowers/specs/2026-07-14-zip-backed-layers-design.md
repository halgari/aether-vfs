# Zip-Backed Layers — Serve Mod Files Directly From Stored ZIP Archives

**Date:** 2026-07-14
**Status:** Approved (brainstorm), ready for planning.

## Goal

Load game/mod layers directly out of ZIP archives in `C:\GameLayers` **without
extracting them to disk**. A file a mod ships inside a zip must be readable by
the target process as if it were a normal file, with the bytes coming straight
from a window inside the zip. Prove it end-to-end from the real archives.

Concretely, `C:\GameLayers` holds three archives that become three VFS layers:

| Layer (bottom→top) | Archive | Contents |
|---|---|---|
| 1 | `1. Skyrim Special Edition.zip` (16 GB) | base game: `SkyrimSE.exe`, `Data/*.bsa`, `Data/*.esm`, … |
| 2 | `2. SKSE 2.2.6.zip` | `skse64_loader.exe`, `skse64_*.dll`, `Data/Scripts/*.pex` |
| 3 | `3. SkyUI 6.11.zip` | `Data/SkyUI_SE.bsa`, `Data/SkyUI_SE.esp` |

## Decisive fact

**Every entry in all three archives is `Stored` (0% compression).** Each file's
bytes are contiguous and uncompressed inside the zip at a known offset. We never
decompress: a Stored entry is simply a byte-window `[data_offset, data_offset +
size)` of the real zip file. Deflated entries are **out of scope** (none exist
here); the reader rejects them rather than silently mis-serving.

The base zip is 16 GB with entries past the 4 GB mark, so it is a **ZIP64**
archive: offsets are 64-bit and the reader must handle the ZIP64 EOCD / extra
fields.

## Constraints

- Zero extra disk footprint: nothing is extracted or copied out of the zips.
  (The eventual game-launch milestone may materialize *only* the bootstrap
  executables — see Deferred — because Windows `CreateProcess` needs a real
  image file. Moddable `Data/` content is never materialized.)
- The shim never parses zips. All zip parsing happens director-side at
  snapshot-build time; the shim receives only resolved windows.
- Backward compatible: existing disk-path (`Redirect`) sources keep working
  unchanged; disk-backed and zip-backed layers can coexist.
- Framing: game modding (USVFS/MO2 successor), not security research.
- Stable Rust, existing workspace conventions.

## Architecture

### A. Source encoding — a tagged `source` blob (`vfs-core`)

Today a file node's `source` blob is a raw UTF-8 disk path; `vfs-redirect::
render_nt` turns it into `\??\…`. We make the blob a tagged union. A raw disk
path never begins with a NUL byte, which gives an unambiguous discriminant:

- **Disk source** (unchanged): `source` = raw UTF-8 path. `decide` →
  `Redirect { target_nt }`, exactly as today.
- **Zip-window source**: `source` = `[0x00][u64 LE data_offset][container path
  UTF-8]`. The entry length is the node's existing `size` field (not repeated in
  the blob). `decide` → new `Decision::Serve { container_nt, offset, length }`.

Encode/decode live in a new `vfs-core::source` module (shared by the producer
`vfs-zip` and the consumer `vfs-redirect`). Functions:

```rust
pub fn encode_zip_window(data_offset: u64, container_path: &str) -> Vec<u8>;
pub enum Source<'a> { Disk(&'a [u8]), ZipWindow { offset: u64, container: &'a [u8] } }
pub fn decode(source: &[u8]) -> Source<'_>;   // NUL-tag discriminated
```

### B. `Decision::Serve` (`vfs-redirect`)

```rust
pub enum Decision {
    PassThrough,
    Redirect { target_nt: String },
    Serve { container_nt: String, offset: u64, length: u64 },  // NEW
    Deny,
}
```

`decide` gains one branch: when the resolved `source` decodes as a zip-window,
return `Serve { container_nt: render_nt(container), offset, length: size }`.
`container_nt` is the zip's NT DOS-device path (`\??\C:\GameLayers\1. …zip`),
reusing `render_nt`. Fail-safe behavior is unchanged for every other case.

### C. `vfs-zip` — zip reader / layer builder (new crate, director side)

Given a zip path, parse the (ZIP64) End-of-Central-Directory and central
directory. For each entry:

1. Require compression method `Stored` (else error).
2. Read the entry's **local file header** to compute the true data offset:
   `data_offset = local_header_off + 30 + local_name_len + local_extra_len`
   (local name/extra lengths can differ from the central directory — the local
   header is authoritative).
3. Emit an `InputEntry { vpath, kind: File, source: encode_zip_window(data_offset,
   zip_path), size, mtime }`. Directory entries (trailing `/`, size 0) become
   `Dir` entries or are implied by the tree builder.

Public API roughly `fn read_layer(zip_path: &Path, layer_id: LayerId) ->
Result<Layer, ZipError>`. Pure parsing + `std::fs` reads; safe Rust
(`#![forbid(unsafe_code)]`). Depends on `vfs-core`.

The director composes the three layers (base zip bottom, SKSE, SkyUI on top;
later layers win), builds the `VfsTree`, and flattens it to a snapshot exactly
as it does today for disk layers.

### D. Shim serving engine — synthetic handles (`vfs-shim`)

On a `Serve` decision, the open hooks **bypass the trampoline** and hand back a
synthetic handle backed by a memory-mapped window of the zip.

- **Zip mapping cache:** a global map `container_nt → base_ptr` created lazily on
  first `Serve` for that container. The zip is opened read-only and mapped once
  (whole-file view; pages fault in lazily from the OS cache). Serving reads is a
  `memcpy` from `base_ptr + offset + position`.
- **Synthetic handle table:** global map `synthetic_handle → { window_ptr:
  base+offset, length, position }`. Handle values are drawn from a tagged
  high-bit range so every hook distinguishes them from real kernel handles by a
  cheap mask test, with no table lookup on the common (real-handle) path.

Hook changes (all gated on "is this handle synthetic?"):

- **`NtCreateFile` / `NtOpenFile` (extend):** on `Serve`, ensure the container is
  mapped, allocate a synthetic handle + table entry (position 0), write it to the
  caller's `FileHandle` out-param, set `IoStatusBlock` (`FILE_OPENED`), return
  `STATUS_SUCCESS`. Do not call the trampoline.
- **`NtReadFile` (NEW hook):** synthetic handle → `memcpy` `Length` bytes from the
  window at the explicit `ByteOffset` (if provided) else current position; clamp
  to `length`; advance position; set `IoStatusBlock.Information` = bytes copied;
  return `STATUS_SUCCESS`, or `STATUS_END_OF_FILE` when at/over the end. Signal
  the event/APC/`IoStatusBlock` as a synchronous completion. Real handles →
  trampoline.
- **`NtQueryInformationFile` (extend):** synthetic handle → serve
  `FileStandardInformation` (EndOfFile = AllocationSize = length, Directory =
  FALSE), `FileEndOfFileInformation`, `FilePositionInformation` (current
  position), `FileNetworkOpenInformation`, `FileBasicInformation` (normal-file
  attributes, mtime from the node). Unhandled classes → `STATUS_NOT_IMPLEMENTED`
  (revisit if a real consumer needs one).
- **`NtSetInformationFile` (extend):** synthetic handle + `FilePositionInformation`
  → update the table entry's position. Other classes on synthetic handles →
  `STATUS_SUCCESS` no-op or `STATUS_NOT_IMPLEMENTED` as appropriate.
- **`NtClose` (extend):** synthetic handle → remove the table entry, return
  `STATUS_SUCCESS`; never call the real close on a synthetic handle. (The zip
  mapping stays cached for the process lifetime.)
- **Path-based attribute queries** (`NtQueryAttributesFile`,
  `NtQueryFullAttributesFile`): already answer from the snapshot's `size`/`mtime`
  via the existing attr hooks — a zip entry reports correct attributes **before**
  open with no new work.

The synthetic-handle bookkeeping is the only new `unsafe` surface and stays
confined to `hook.rs` alongside the existing detours.

## Deferred (explicitly out of scope for this work)

- **Memory-mapped consumers.** If a consumer calls `NtCreateSection` /
  `NtMapViewOfSection` on a synthetic handle, those are not hooked and the call
  fails. Not implemented now (user decision). If the game-launch milestone shows
  Skyrim memory-maps its BSAs, the Stored layout still permits serving a section
  by mapping the real zip at a 64 KB-aligned base and returning a view pointer
  offset into the entry — future M7 work, verified empirically first.
- **Launching an executable that lives inside a zip.** Windows maps the main
  image before our shim exists in the target, so bootstrap executables must be
  real files. The game milestone materializes only the loader/exe (or uses an
  existing real install) and overlays all of `Data/` from the zips. Not needed
  for the automated proof.
- **Deflated entries / decompression.** None exist in these archives; the reader
  errors on non-Stored methods.

## Slicing

1. **`vfs-core::source` codec + `vfs-zip` reader.** Pure/std. Unit-tested against
   the three real archives: ZIP64 parsing, Stored enforcement, correct data
   offsets (spot-check a known entry's bytes/CRC), correct `vpath`/`size`/`mtime`.
   No shim changes.
2. **`Decision::Serve` in `vfs-redirect`.** Decode zip-window sources; unit tests
   over a fixture snapshot. Disk sources unchanged.
3. **Shim synthetic-serving engine.** Zip mapping cache, synthetic handle table,
   `NtReadFile` hook, extended query/set/close/open hooks. In-process integration
   test: `std::fs::read` of a virtual path whose source is a window into a small
   real test zip returns the exact entry bytes.
4. **Cross-process automated proof (acceptance #1).** A director builds a snapshot
   from the real `C:\GameLayers` zips; an injected probe reads `Data/SkyUI_SE.esp`
   and a slice of a large `.bsa` byte-for-byte straight from the zip; assert bytes
   + CRC-32 match the zip's central-directory CRC, and that nothing was extracted.

## Acceptance criteria

- Reading a Stored zip entry through the injected shim returns bytes identical to
  the archive's stored content (verified against the entry's ZIP64 CRC-32).
- Seeking (set/query `FilePositionInformation`) and size queries
  (`FileStandardInformation`) report the entry window, not the whole zip.
- No file is extracted or copied out of any zip during the test.
- `cargo test --workspace` green; disk-backed redirect tests still pass.

## Risks

- **ZIP64 / local-header offset math** is the fiddly part of `vfs-zip`; verify a
  known entry's first bytes against a direct `unzip -p` of the same entry.
- **`NtReadFile` completion semantics** (IoStatusBlock, event/APC signaling,
  synchronous vs. overlapped) must mimic a real synchronous read closely enough
  for the caller's wait logic. Test with real synchronous reads first.
- **Synthetic handle collisions / leakage** — a synthetic handle passed to an
  un-hooked syscall fails. Covered set (read/query/set/close) matches `ReadFile`
  access; mmap is the known gap (deferred above).
