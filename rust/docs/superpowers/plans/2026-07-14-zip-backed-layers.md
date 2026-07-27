# Zip-Backed Layers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve mod/game files directly out of `Stored` ZIP archives (no extraction) by teaching the resolver about zip-window sources and giving the shim a synthetic-handle read path backed by a memory-mapped zip.

**Architecture:** A file node's `source` blob becomes a tagged union: a raw disk path (today) or a zip-window `[0x00][u64 offset][container path]`. `vfs-redirect::decide` turns a zip-window into a new `Decision::Serve { container_nt, offset, length }`. A director-side `vfs-zip` crate builds layers from a zip's (ZIP64) central directory. In the target, the open hooks answer `Serve` by memory-mapping the zip once and returning a tagged synthetic handle; a new `NtReadFile` hook `memcpy`s bytes from the window; query/set/close hooks recognize synthetic handles.

**Tech Stack:** Rust (stable), windows-sys 0.59, retour 0.3, existing `vfs-core`/`vfs-shared`/`vfs-redirect`/`vfs-shim`/`vfs-inject` crates.

## Global Constraints

- Stable Rust; existing workspace conventions.
- Zero extraction: no file content is ever copied out of a zip to disk.
- Only `Stored` (uncompressed) zip entries are supported; a non-`Stored` entry is a hard error in `vfs-zip`. No decompression code.
- Offsets are `u64`; the base archive is a ZIP64 file (16 GB, entries past 4 GB). `vfs-zip` must parse the ZIP64 EOCD + ZIP64 extra fields.
- The shim never parses zips — it receives resolved `(container, offset, length)` only.
- Backward compatible: raw-disk-path `source` blobs still produce `Decision::Redirect` unchanged.
- All new `unsafe` in the shim stays in `hook.rs` / a new `zipserve.rs` module.
- Framing: game modding, not security research.
- Source blob discriminant: a raw disk path never begins with a NUL byte; a `0x00` first byte marks a zip-window blob.

---

### Task 1: `vfs-core::source` — tagged source-blob codec

**Files:**
- Create: `crates/vfs-core/src/source.rs`
- Modify: `crates/vfs-core/src/lib.rs` (add `pub mod source;` and re-exports)

**Interfaces:**
- Produces:
  - `pub fn encode_zip_window(offset: u64, container: &str) -> Vec<u8>`
  - `pub enum Source<'a> { Disk(&'a [u8]), ZipWindow { offset: u64, container: &'a [u8] } }`
  - `pub fn decode(blob: &[u8]) -> Source<'_>`
  - Re-exported from `vfs_core` as `vfs_core::{encode_zip_window, decode, Source}`.

- [ ] **Step 1: Write the failing test**

Append to `crates/vfs-core/src/source.rs` (create the file with this test first):

```rust
//! Tagged encoding of a file node's `source` blob: either a raw UTF-8 disk
//! path (a path never starts with NUL) or a zip-window `[0x00][u64 LE
//! offset][container path UTF-8]`.

/// Marks a zip-window blob. A raw disk path never begins with NUL.
const ZIP_TAG: u8 = 0x00;

/// A decoded `source` blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source<'a> {
    /// Raw UTF-8 disk path (legacy / disk-backed layers).
    Disk(&'a [u8]),
    /// A contiguous window inside a Stored zip entry.
    ZipWindow { offset: u64, container: &'a [u8] },
}

/// Encode a zip-window source: `[0x00][u64 LE offset][container path bytes]`.
pub fn encode_zip_window(offset: u64, container: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(9 + container.len());
    v.push(ZIP_TAG);
    v.extend_from_slice(&offset.to_le_bytes());
    v.extend_from_slice(container.as_bytes());
    v
}

/// Decode a `source` blob. A leading NUL selects a zip-window; anything else
/// (including empty) is a raw disk path. Malformed zip-window blobs (too short)
/// fall back to `Disk` so callers stay fail-safe.
pub fn decode(blob: &[u8]) -> Source<'_> {
    if blob.first() == Some(&ZIP_TAG) && blob.len() >= 9 {
        let mut off = [0u8; 8];
        off.copy_from_slice(&blob[1..9]);
        Source::ZipWindow { offset: u64::from_le_bytes(off), container: &blob[9..] }
    } else {
        Source::Disk(blob)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_zip_window() {
        let blob = encode_zip_window(0x1_0000_0007, r"C:\GameLayers\base.zip");
        assert_eq!(
            decode(&blob),
            Source::ZipWindow { offset: 0x1_0000_0007, container: br"C:\GameLayers\base.zip" }
        );
    }

    #[test]
    fn a_plain_path_decodes_as_disk() {
        assert_eq!(decode(br"D:\Mods\Cool\foo.esp"), Source::Disk(br"D:\Mods\Cool\foo.esp"));
    }

    #[test]
    fn a_truncated_zip_blob_is_treated_as_disk() {
        // Leading NUL but fewer than 9 bytes -> not a valid window.
        assert_eq!(decode(&[0x00, 0x01, 0x02]), Source::Disk(&[0x00, 0x01, 0x02]));
    }
}
```

- [ ] **Step 2: Wire the module**

In `crates/vfs-core/src/lib.rs`, add near the other `pub mod`/`pub use` lines:

```rust
pub mod source;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p vfs-core source::`
Expected: PASS (3 tests).

- [ ] **Step 4: Commit**

```bash
git add crates/vfs-core/src/source.rs crates/vfs-core/src/lib.rs
git commit -m "feat(vfs-core): tagged source-blob codec for zip-window sources"
```

---

### Task 2: `vfs-zip` — ZIP64 central-directory reader / layer builder

**Files:**
- Create: `crates/vfs-zip/Cargo.toml`
- Create: `crates/vfs-zip/src/lib.rs`
- Modify: `Cargo.toml` (workspace `members` — add `"crates/vfs-zip"`)

**Interfaces:**
- Consumes: `vfs_core::{InputEntry, EntryKind, Layer, LayerId, SourceId}`, `vfs_core::encode_zip_window`.
- Produces:
  - `pub fn read_layer(zip_path: &std::path::Path, id: vfs_core::LayerId) -> Result<vfs_core::Layer, ZipError>`
  - `pub enum ZipError { Io(std::io::Error), NotAZip, Unsupported(String), Malformed(String) }`

**Reference — ZIP structures this task parses (little-endian):**
- End of Central Directory (EOCD), signature `0x06054b50`, found by scanning the last ≤ 65 557 bytes backward. Fields we need: total entries (u16 @16), central-dir offset (u32 @16→ offset field @16? see below), and whether ZIP64 is in play (any 0xFFFF/0xFFFFFFFF sentinel).
- ZIP64 EOCD locator, signature `0x07064b50` (20 bytes, immediately before the EOCD when ZIP64): u64 offset of the ZIP64 EOCD record @8.
- ZIP64 EOCD record, signature `0x06064b50`: total entries u64 @32, central-dir size u64 @40, central-dir offset u64 @48.
- Central directory header, signature `0x02014b50` (46 bytes fixed): compression method u16 @10, mod time u16 @12, mod date u16 @14, crc32 u32 @16, comp size u32 @20, uncomp size u32 @24, name len u16 @28, extra len u16 @30, comment len u16 @32, local header offset u32 @42, then name/extra/comment. ZIP64 extra field (header id `0x0001`) supplies real u64 values for any field whose 32-bit slot is `0xFFFFFFFF`, in the order: uncomp size, comp size, local header offset (only those that are sentinel).
- Local file header, signature `0x04034b50` (30 bytes fixed): name len u16 @26, extra len u16 @28. **Data offset = local_header_offset + 30 + local_name_len + local_extra_len** (local lengths are authoritative, may differ from central).
- Compression method `0` = Stored (required). Anything else → `Unsupported`.
- DOS date/time → convert to a plain `mtime` (see helper below; a monotone-ish integer is sufficient for the VFS).

- [ ] **Step 1: Create the crate manifest**

Create `crates/vfs-zip/Cargo.toml`:

```toml
[package]
name = "vfs-zip"
version = "0.1.0"
edition = "2021"
publish = false

[dependencies]
vfs-core = { path = "../vfs-core" }
```

Add `"crates/vfs-zip"` to the `members` array in the workspace root `Cargo.toml`.

- [ ] **Step 2: Write the failing test + the reader**

Create `crates/vfs-zip/src/lib.rs`:

```rust
//! Read a ZIP archive's central directory (ZIP64-aware) and emit a `vfs-core`
//! layer whose file entries are zip-window sources. Only `Stored` entries are
//! supported — nothing is decompressed or extracted.
#![forbid(unsafe_code)]

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use vfs_core::encode_zip_window;
use vfs_core::{EntryKind, InputEntry, Layer, LayerId, SourceId};

#[derive(Debug)]
pub enum ZipError {
    Io(std::io::Error),
    NotAZip,
    Unsupported(String),
    Malformed(String),
}

impl From<std::io::Error> for ZipError {
    fn from(e: std::io::Error) -> Self {
        ZipError::Io(e)
    }
}

const EOCD_SIG: u32 = 0x0605_4b50;
const EOCD64_LOC_SIG: u32 = 0x0706_4b50;
const EOCD64_SIG: u32 = 0x0606_4b50;
const CDH_SIG: u32 = 0x0201_4b50;
const LFH_SIG: u32 = 0x0403_4b50;
const SENTINEL32: u32 = 0xFFFF_FFFF;

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64le(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// DOS date/time (both u16) -> a plain sortable mtime. Not a real timestamp;
/// the VFS only needs a stable integer.
fn dos_mtime(date: u16, time: u16) -> i64 {
    ((date as i64) << 16) | (time as i64)
}

/// One central-directory entry after ZIP64 fixups.
struct CdEntry {
    name: String,
    method: u16,
    crc32: u32,
    uncomp_size: u64,
    local_header_off: u64,
    mtime: i64,
}

/// Read the whole file into memory? No — the base zip is 16 GB. We seek and read
/// only the directory + local headers. This opens the file for the lifetime of
/// the call.
pub fn read_layer(zip_path: &Path, id: LayerId) -> Result<Layer, ZipError> {
    let mut f = std::fs::File::open(zip_path)?;
    let file_len = f.metadata()?.len();
    let container = zip_path.to_string_lossy().to_string();

    let (cd_off, cd_count) = locate_central_directory(&mut f, file_len)?;
    let entries = read_central_directory(&mut f, cd_off, cd_count)?;

    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        // Directory entries (trailing '/') become Dir nodes; the tree builder
        // also implies parents, but emitting them keeps empty dirs.
        if e.name.ends_with('/') {
            out.push(InputEntry {
                vpath: e.name.trim_end_matches('/').to_string(),
                kind: EntryKind::Dir,
                source: SourceId::new(Vec::new()),
                size: 0,
                mtime: e.mtime,
            });
            continue;
        }
        if e.method != 0 {
            return Err(ZipError::Unsupported(format!(
                "entry {} uses compression method {} (only Stored is supported)",
                e.name, e.method
            )));
        }
        let data_off = data_offset(&mut f, e.local_header_off)?;
        out.push(InputEntry {
            vpath: e.name,
            kind: EntryKind::File,
            source: SourceId::new(encode_zip_window(data_off, &container)),
            size: e.uncomp_size,
            mtime: e.mtime,
        });
        let _ = e.crc32; // reserved for future integrity checks
    }
    Ok(Layer { id, entries: out })
}

/// Find the central directory offset + entry count, honoring ZIP64.
fn locate_central_directory(
    f: &mut std::fs::File,
    file_len: u64,
) -> Result<(u64, u64), ZipError> {
    // EOCD is within the last 22 + 65535 bytes. Scan backward for its signature.
    let scan = 22 + 0xFFFF;
    let start = file_len.saturating_sub(scan);
    let mut buf = vec![0u8; (file_len - start) as usize];
    f.seek(SeekFrom::Start(start))?;
    f.read_exact(&mut buf)?;

    let eocd = (0..=buf.len().saturating_sub(22))
        .rev()
        .find(|&i| u32le(&buf, i) == EOCD_SIG)
        .ok_or(ZipError::NotAZip)?;

    let mut count = u16le(&buf, eocd + 10) as u64;
    let mut cd_off = u32le(&buf, eocd + 16) as u64;

    // ZIP64: a locator sits 20 bytes before the EOCD.
    if (count == 0xFFFF || cd_off == SENTINEL32 as u64) && eocd >= 20 {
        let loc = eocd - 20;
        if u32le(&buf, loc) == EOCD64_LOC_SIG {
            let eocd64_off = u64le(&buf, loc + 8);
            let mut rec = [0u8; 56];
            f.seek(SeekFrom::Start(eocd64_off))?;
            f.read_exact(&mut rec)?;
            if u32le(&rec, 0) != EOCD64_SIG {
                return Err(ZipError::Malformed("bad ZIP64 EOCD signature".into()));
            }
            count = u64le(&rec, 32);
            cd_off = u64le(&rec, 48);
        }
    }
    Ok((cd_off, count))
}

/// Read `count` central-directory headers starting at `cd_off`.
fn read_central_directory(
    f: &mut std::fs::File,
    cd_off: u64,
    count: u64,
) -> Result<Vec<CdEntry>, ZipError> {
    f.seek(SeekFrom::Start(cd_off))?;
    let mut entries = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut fixed = [0u8; 46];
        f.read_exact(&mut fixed)?;
        if u32le(&fixed, 0) != CDH_SIG {
            return Err(ZipError::Malformed("bad central-directory signature".into()));
        }
        let method = u16le(&fixed, 10);
        let time = u16le(&fixed, 12);
        let date = u16le(&fixed, 14);
        let crc32 = u32le(&fixed, 16);
        let mut uncomp_size = u32le(&fixed, 24) as u64;
        let name_len = u16le(&fixed, 28) as usize;
        let extra_len = u16le(&fixed, 30) as usize;
        let comment_len = u16le(&fixed, 32) as usize;
        let mut local_header_off = u32le(&fixed, 42) as u64;

        let mut var = vec![0u8; name_len + extra_len + comment_len];
        f.read_exact(&mut var)?;
        let name = String::from_utf8_lossy(&var[..name_len]).replace('\\', "/");
        let extra = &var[name_len..name_len + extra_len];

        // ZIP64 extra field: real u64s for any sentinel 32-bit field, in order
        // uncomp, comp, local-header-offset.
        if uncomp_size == SENTINEL32 as u64 || local_header_off == SENTINEL32 as u64 {
            apply_zip64_extra(
                extra,
                &fixed,
                &mut uncomp_size,
                &mut local_header_off,
            )?;
        }

        entries.push(CdEntry {
            name,
            method,
            crc32,
            uncomp_size,
            local_header_off,
            mtime: dos_mtime(date, time),
        });
    }
    Ok(entries)
}

/// Walk extra fields for header id 0x0001 and overwrite sentinel values, in the
/// canonical order: uncompressed, compressed, local-header offset, disk#.
fn apply_zip64_extra(
    extra: &[u8],
    fixed: &[u8],
    uncomp_size: &mut u64,
    local_header_off: &mut u64,
) -> Result<(), ZipError> {
    let comp_is_sentinel = u32le(fixed, 20) == SENTINEL32;
    let uncomp_is_sentinel = u32le(fixed, 24) == SENTINEL32;
    let off_is_sentinel = u32le(fixed, 42) == SENTINEL32;

    let mut i = 0usize;
    while i + 4 <= extra.len() {
        let id = u16le(extra, i);
        let sz = u16le(extra, i + 2) as usize;
        let body_start = i + 4;
        if body_start + sz > extra.len() {
            break;
        }
        if id == 0x0001 {
            let body = &extra[body_start..body_start + sz];
            let mut p = 0usize;
            if uncomp_is_sentinel && p + 8 <= body.len() {
                *uncomp_size = u64le(body, p);
                p += 8;
            }
            if comp_is_sentinel && p + 8 <= body.len() {
                p += 8; // compressed size — unused (Stored == uncomp)
            }
            if off_is_sentinel && p + 8 <= body.len() {
                *local_header_off = u64le(body, p);
            }
            return Ok(());
        }
        i = body_start + sz;
    }
    Ok(())
}

/// Read a local file header to compute the true data offset.
fn data_offset(f: &mut std::fs::File, local_header_off: u64) -> Result<u64, ZipError> {
    let mut lfh = [0u8; 30];
    f.seek(SeekFrom::Start(local_header_off))?;
    f.read_exact(&mut lfh)?;
    if u32le(&lfh, 0) != LFH_SIG {
        return Err(ZipError::Malformed("bad local file header signature".into()));
    }
    let name_len = u16le(&lfh, 26) as u64;
    let extra_len = u16le(&lfh, 28) as u64;
    Ok(local_header_off + 30 + name_len + extra_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write a minimal single-entry Stored zip (no ZIP64) and return its path.
    fn write_plain_zip(dir: &Path, name: &str, content: &[u8]) -> std::path::PathBuf {
        let path = dir.join("plain.zip");
        let mut f = std::fs::File::create(&path).unwrap();
        let mut buf = Vec::new();
        let crc = crc32(content);
        let n = name.len() as u16;
        // Local file header.
        buf.extend_from_slice(&LFH_SIG.to_le_bytes());
        buf.extend_from_slice(&[0u8; 2]); // version
        buf.extend_from_slice(&[0u8; 2]); // flags
        buf.extend_from_slice(&0u16.to_le_bytes()); // method Stored
        buf.extend_from_slice(&0u16.to_le_bytes()); // time
        buf.extend_from_slice(&0u16.to_le_bytes()); // date
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
        buf.extend_from_slice(&n.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // extra len
        buf.extend_from_slice(name.as_bytes());
        let data_off = buf.len() as u32;
        buf.extend_from_slice(content);
        let cd_off = buf.len() as u32;
        // Central directory header.
        buf.extend_from_slice(&CDH_SIG.to_le_bytes());
        buf.extend_from_slice(&[0u8; 2]); // version made by
        buf.extend_from_slice(&[0u8; 2]); // version needed
        buf.extend_from_slice(&[0u8; 2]); // flags
        buf.extend_from_slice(&0u16.to_le_bytes()); // method
        buf.extend_from_slice(&0u16.to_le_bytes()); // time
        buf.extend_from_slice(&0u16.to_le_bytes()); // date
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
        buf.extend_from_slice(&n.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // extra
        buf.extend_from_slice(&0u16.to_le_bytes()); // comment
        buf.extend_from_slice(&[0u8; 2]); // disk start
        buf.extend_from_slice(&[0u8; 2]); // internal attrs
        buf.extend_from_slice(&[0u8; 4]); // external attrs
        buf.extend_from_slice(&0u32.to_le_bytes()); // local header offset (0)
        buf.extend_from_slice(name.as_bytes());
        let cd_size = buf.len() as u32 - cd_off;
        // EOCD.
        buf.extend_from_slice(&EOCD_SIG.to_le_bytes());
        buf.extend_from_slice(&[0u8; 2]); // disk
        buf.extend_from_slice(&[0u8; 2]); // cd disk
        buf.extend_from_slice(&1u16.to_le_bytes()); // entries on disk
        buf.extend_from_slice(&1u16.to_le_bytes()); // total entries
        buf.extend_from_slice(&cd_size.to_le_bytes());
        buf.extend_from_slice(&cd_off.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // comment len
        f.write_all(&buf).unwrap();
        let _ = data_off;
        path
    }

    /// Tiny CRC-32 (IEEE) so the fixture is self-contained.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    #[test]
    fn reads_a_plain_stored_zip_entry() {
        let dir = std::env::temp_dir().join(format!("vfs-zip-plain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let content = b"HELLO FROM INSIDE THE ZIP";
        let zip = write_plain_zip(&dir, "Data/hello.txt", content);

        let layer = read_layer(&zip, LayerId(0)).unwrap();
        let entry = layer.entries.iter().find(|e| e.vpath == "Data/hello.txt").unwrap();
        assert_eq!(entry.kind, EntryKind::File);
        assert_eq!(entry.size, content.len() as u64);

        // The recorded data offset must point at the entry's bytes on disk.
        let win = vfs_core::decode(&entry.source.0);
        match win {
            vfs_core::Source::ZipWindow { offset, .. } => {
                let mut f = std::fs::File::open(&zip).unwrap();
                let mut got = vec![0u8; content.len()];
                f.seek(SeekFrom::Start(offset)).unwrap();
                f.read_exact(&mut got).unwrap();
                assert_eq!(&got, content);
            }
            other => panic!("expected zip window, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_deflated_entry() {
        // method != 0 in the central header -> Unsupported. Reuse the plain
        // writer but flip the method byte at central offset (10) after writing.
        let dir = std::env::temp_dir().join(format!("vfs-zip-defl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let zip = write_plain_zip(&dir, "a.bin", b"xxxx");
        let mut bytes = std::fs::read(&zip).unwrap();
        // Find the central-directory signature and set method (offset +10) to 8.
        let cd = bytes
            .windows(4)
            .position(|w| w == CDH_SIG.to_le_bytes())
            .unwrap();
        bytes[cd + 10] = 8;
        std::fs::write(&zip, &bytes).unwrap();
        assert!(matches!(read_layer(&zip, LayerId(0)), Err(ZipError::Unsupported(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p vfs-zip`
Expected: PASS (2 tests). If `SourceId` field access `entry.source.0` fails to compile, confirm `SourceId(pub Box<[u8]>)` — it is public in `vfs-core::model`.

- [ ] **Step 4: Real-archive smoke check (ignored by default)**

Append this test to the `tests` module (validates parsing against a real, non-fixture archive — SkyUI is only ~3 MB so it is safe to keep un-ignored; the 16 GB ZIP64 base is `#[ignore]`):

```rust
    #[test]
    fn reads_the_real_skyui_archive() {
        let zip = Path::new(r"C:\GameLayers\3. SkyUI 6.11.zip");
        if !zip.exists() {
            return; // skip when the archive is absent
        }
        let layer = read_layer(zip, LayerId(2)).unwrap();
        let esp = layer.entries.iter().find(|e| e.vpath == "Data/SkyUI_SE.esp").unwrap();
        assert_eq!(esp.size, 2433);
    }

    #[test]
    #[ignore = "reads the 16 GB ZIP64 base archive; run manually"]
    fn reads_the_real_base_archive_zip64() {
        let zip = Path::new(r"C:\GameLayers\1. Skyrim Special Edition.zip");
        let layer = read_layer(zip, LayerId(0)).unwrap();
        // An entry known to sit past the 4 GB mark exercises ZIP64 offsets.
        let tex = layer.entries.iter().find(|e| e.vpath == "Data/Skyrim - Textures1.bsa").unwrap();
        assert_eq!(tex.size, 1_511_492_648);
        let win = vfs_core::decode(&tex.source.0);
        if let vfs_core::Source::ZipWindow { offset, .. } = win {
            assert!(offset > 0xFFFF_FFFF, "expected a 64-bit offset");
        }
    }
```

Run: `cargo test -p vfs-zip` (SkyUI test runs if the archive exists).
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/vfs-zip Cargo.toml
git commit -m "feat(vfs-zip): ZIP64 central-directory reader emitting zip-window layers"
```

---

### Task 3: `Decision::Serve` in `vfs-redirect`

**Files:**
- Modify: `crates/vfs-redirect/src/lib.rs` (the `Decision` enum ~line 193, and `decide` ~line 50-60)

**Interfaces:**
- Consumes: `vfs_core::{decode, Source}`, existing `render_nt`.
- Produces: new variant `Decision::Serve { container_nt: String, offset: u64, length: u64 }`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/vfs-redirect/src/lib.rs` (reuse the existing `file` fixture helper pattern; a zip-window source is built with `vfs_core::encode_zip_window`):

```rust
    #[test]
    fn decide_serves_a_zip_window_source() {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId, SourceId};
        let src = SourceId::new(vfs_core::encode_zip_window(
            0x1_0000_0010,
            r"C:\GameLayers\base.zip",
        ));
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/big.bsa".into(),
                kind: EntryKind::File,
                source: src,
                size: 4242,
                mtime: 1,
            }],
        }])
        .unwrap();
        let snap = vfs_shared::bridge::flatten(&tree);
        let reader = vfs_shared::SnapshotReader::open(&snap).unwrap();
        let map = RootMap::new(r"\??\C:\Games\Skyrim").unwrap();
        assert_eq!(
            map.decide(r"\??\C:\Games\Skyrim\Data\big.bsa", &reader),
            Decision::Serve {
                container_nt: r"\??\C:\GameLayers\base.zip".to_string(),
                offset: 0x1_0000_0010,
                length: 4242,
            }
        );
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p vfs-redirect decide_serves_a_zip_window_source`
Expected: FAIL — `Decision` has no `Serve` variant.

- [ ] **Step 3: Add the variant**

In the `Decision` enum (~line 193) add:

```rust
    /// Serve the file's bytes from a window inside a container (zip) file.
    /// The shim opens `container_nt`, maps it, and returns a synthetic handle
    /// covering `[offset, offset + length)`.
    Serve { container_nt: String, offset: u64, length: u64 },
```

- [ ] **Step 4: Branch in `decide`**

Replace the `File` arm of `decide` (currently lines 52-54) with:

```rust
            Located::Resolved(SnapResolution::File { source, size, .. }) => {
                match vfs_core::decode(&source) {
                    vfs_core::Source::ZipWindow { offset, container } => Decision::Serve {
                        container_nt: render_nt(container),
                        offset,
                        length: size,
                    },
                    vfs_core::Source::Disk(bytes) => {
                        Decision::Redirect { target_nt: render_nt(bytes) }
                    }
                }
            }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p vfs-redirect`
Expected: PASS (existing redirect/deny/passthrough tests + the new Serve test). Disk-source tests are unchanged because a plain path decodes to `Source::Disk`.

- [ ] **Step 6: Commit**

```bash
git add crates/vfs-redirect/src/lib.rs
git commit -m "feat(vfs-redirect): Decision::Serve for zip-window sources"
```

---

### Task 4: Shim synthetic-handle serving engine

**Files:**
- Modify: `crates/vfs-shim/Cargo.toml` (windows-sys features: add `Win32_Storage_FileSystem`, ensure `Win32_System_Memory`, `Win32_System_Threading`)
- Modify: `crates/vfs-shim/src/ntdef.rs` (NtReadFile type, status/class constants, FILE_STANDARD/POSITION structs)
- Create: `crates/vfs-shim/src/zipserve.rs` (zip mmap cache + synthetic handle table)
- Modify: `crates/vfs-shim/src/lib.rs` (add `mod zipserve;`)
- Modify: `crates/vfs-shim/src/hook.rs` (Serve arms, NtReadFile hook, synthetic branches in qif/setinfo/close, install NtReadFile detour)

**Interfaces:**
- Consumes: `vfs_redirect::Decision::Serve`, `ntdef` additions.
- Produces (in `zipserve`):
  - `pub fn open_synth(container_nt: &str, offset: u64, length: u64) -> Option<isize>` — maps the container (cached), registers a window, returns a tagged synthetic handle value (as `isize`).
  - `pub fn is_synth(handle: isize) -> bool`
  - `pub fn read(handle: isize, want: usize, explicit_off: Option<u64>) -> Option<(Vec<u8>, u64, bool)>` — returns `(bytes, new_position, at_eof)`.
  - `pub fn size(handle: isize) -> Option<u64>`
  - `pub fn position(handle: isize) -> Option<u64>`
  - `pub fn set_position(handle: isize, pos: u64) -> bool`
  - `pub fn close(handle: isize) -> bool`

- [ ] **Step 1: Add NT type defs + constants (ntdef.rs)**

Append to `crates/vfs-shim/src/ntdef.rs`:

```rust
/// `ntdll!NtReadFile`. `Event`/`ApcRoutine`/`ApcContext`/`Key` are unused by
/// synchronous callers; `ByteOffset` is a `PLARGE_INTEGER` (nullable).
pub type NtReadFileFn = unsafe extern "system" fn(
    HANDLE,        // FileHandle
    HANDLE,        // Event
    *const c_void, // ApcRoutine
    *const c_void, // ApcContext
    *mut c_void,   // IoStatusBlock
    *mut c_void,   // Buffer
    u32,           // Length
    *const i64,    // ByteOffset (LARGE_INTEGER)
    *const u32,    // Key
) -> NTSTATUS;

/// `STATUS_END_OF_FILE`.
pub const STATUS_END_OF_FILE: NTSTATUS = 0xC000_0011u32 as i32;
/// `STATUS_NOT_IMPLEMENTED`.
pub const STATUS_NOT_IMPLEMENTED: NTSTATUS = 0xC000_0002u32 as i32;
/// `FILE_OPENED` disposition-information for a synthetic open's IoStatusBlock.
pub const FILE_OPENED: usize = 1;

/// `FileStandardInformation` (class 5).
pub const FILE_STANDARD_INFORMATION: u32 = 5;
/// `FilePositionInformation` (class 14).
pub const FILE_POSITION_INFORMATION: u32 = 14;

/// Layout-compatible with `FILE_STANDARD_INFORMATION` (24 bytes).
#[repr(C)]
pub struct FileStandardInformation {
    pub allocation_size: i64,
    pub end_of_file: i64,
    pub number_of_links: u32,
    pub delete_pending: u8,
    pub directory: u8,
    pub _pad: u16,
}

/// Layout-compatible with `FILE_POSITION_INFORMATION` (8 bytes).
#[repr(C)]
pub struct FilePositionInformation {
    pub current_byte_offset: i64,
}
```

- [ ] **Step 2: Create the zipserve module with its unit test**

Create `crates/vfs-shim/src/zipserve.rs`:

```rust
//! Serve zip-window bytes from memory-mapped container files behind synthetic
//! file handles. All `unsafe` for mapping lives here.
#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, FILE_MAP_READ, PAGE_READONLY,
};

/// High tag bit (2^46) marking a synthetic handle; real kernel handles never
/// reach this magnitude. The sign bit (2^63) stays clear so the value is a
/// positive handle, never confused with pseudo-handles (-1..-6) or
/// INVALID_HANDLE_VALUE.
const SYNTH_TAG: usize = 0x0000_4000_0000_0000;

/// A mapped container: base address of the whole-file view.
struct ZipMap {
    base: usize,
}

/// A synthetic open: absolute window start (map base + entry offset), length,
/// and current read position.
struct SynthFile {
    window: usize,
    length: u64,
    position: u64,
}

// Raw addresses stored as usize -> Send/Sync-safe in the maps.
static ZIP_MAPS: Mutex<BTreeMap<String, ZipMap>> = Mutex::new(BTreeMap::new());
static SYNTH: Mutex<BTreeMap<usize, SynthFile>> = Mutex::new(BTreeMap::new());
static NEXT_SLOT: Mutex<usize> = Mutex::new(0);

/// Strip a `\??\` / `\\?\` device prefix to a Win32 path for `CreateFileW`.
fn to_win32(nt: &str) -> String {
    nt.strip_prefix(r"\??\").or_else(|| nt.strip_prefix(r"\\?\")).unwrap_or(nt).to_string()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Map `container_nt` once (cached), returning its base address.
fn ensure_mapped(container_nt: &str) -> Option<usize> {
    let mut maps = ZIP_MAPS.lock().ok()?;
    if let Some(m) = maps.get(container_nt) {
        return Some(m.base);
    }
    let win = to_win32(container_nt);
    // SAFETY: standard read-only open + whole-file mapping; handles closed on
    // failure. The view outlives the process (never unmapped).
    unsafe {
        let path = wide(&win);
        let file = CreateFileW(
            path.as_ptr(),
            0x8000_0000, // GENERIC_READ
            FILE_SHARE_READ,
            core::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            core::ptr::null_mut(),
        );
        if file == INVALID_HANDLE_VALUE {
            return None;
        }
        let mapping = CreateFileMappingW(
            file,
            core::ptr::null(),
            PAGE_READONLY,
            0,
            0, // whole file
            core::ptr::null(),
        );
        if mapping.is_null() {
            CloseHandle(file);
            return None;
        }
        let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
        // The section keeps the pages alive while mapped; we can drop the file
        // and mapping handles but keep them for clarity — closing the file is
        // safe once the mapping exists. Keep the mapping handle open.
        CloseHandle(file);
        if view.Value.is_null() {
            CloseHandle(mapping);
            return None;
        }
        let base = view.Value as usize;
        maps.insert(container_nt.to_string(), ZipMap { base });
        Some(base)
    }
}

/// Register a synthetic open over `[offset, offset+length)` of `container_nt`.
pub fn open_synth(container_nt: &str, offset: u64, length: u64) -> Option<isize> {
    let base = ensure_mapped(container_nt)?;
    let window = base.checked_add(offset as usize)?;
    let mut slot = NEXT_SLOT.lock().ok()?;
    let handle = SYNTH_TAG | (*slot << 3);
    *slot += 1;
    drop(slot);
    SYNTH.lock().ok()?.insert(handle, SynthFile { window, length, position: 0 });
    Some(handle as isize)
}

/// Whether `handle` is one of ours.
pub fn is_synth(handle: isize) -> bool {
    (handle as usize) & SYNTH_TAG != 0
}

/// Read up to `want` bytes from `explicit_off` (or the current position).
/// Returns `(bytes, new_position, at_eof)`. `at_eof` is true when the read
/// started at or beyond the end (zero bytes available).
pub fn read(handle: isize, want: usize, explicit_off: Option<u64>) -> Option<(Vec<u8>, u64, bool)> {
    let mut t = SYNTH.lock().ok()?;
    let f = t.get_mut(&(handle as usize))?;
    let start = explicit_off.unwrap_or(f.position);
    if start >= f.length {
        return Some((Vec::new(), start, true));
    }
    let remaining = (f.length - start) as usize;
    let n = want.min(remaining);
    // SAFETY: window..window+length lies inside the mapped view; start+n <= length.
    let src = (f.window + start as usize) as *const u8;
    let bytes = unsafe { core::slice::from_raw_parts(src, n).to_vec() };
    let new_pos = start + n as u64;
    f.position = new_pos;
    Some((bytes, new_pos, false))
}

pub fn size(handle: isize) -> Option<u64> {
    Some(SYNTH.lock().ok()?.get(&(handle as usize))?.length)
}

pub fn position(handle: isize) -> Option<u64> {
    Some(SYNTH.lock().ok()?.get(&(handle as usize))?.position)
}

pub fn set_position(handle: isize, pos: u64) -> bool {
    match SYNTH.lock() {
        Ok(mut t) => match t.get_mut(&(handle as usize)) {
            Some(f) => {
                f.position = pos;
                true
            }
            None => false,
        },
        Err(_) => false,
    }
}

/// Drop a synthetic open. The container mapping stays cached for the process.
pub fn close(handle: isize) -> bool {
    match SYNTH.lock() {
        Ok(mut t) => t.remove(&(handle as usize)).is_some(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn serves_a_window_from_a_real_file() {
        // Build a file whose bytes 5..10 are the window; map + read it.
        let dir = std::env::temp_dir().join(format!("vfs-zipserve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.bin");
        std::fs::File::create(&path).unwrap().write_all(b"AAAAABCDEFGHIJ").unwrap();
        let nt = format!(r"\??\{}", path.to_string_lossy());

        let h = open_synth(&nt, 5, 5).expect("open_synth");
        assert!(is_synth(h));
        assert_eq!(size(h), Some(5));
        let (bytes, pos, eof) = read(h, 3, None).unwrap();
        assert_eq!(&bytes, b"BCD");
        assert_eq!(pos, 3);
        assert!(!eof);
        let (bytes2, _, _) = read(h, 100, None).unwrap();
        assert_eq!(&bytes2, b"EF"); // clamped to the 5-byte window
        let (_, _, eof2) = read(h, 1, None).unwrap();
        assert!(eof2);
        assert!(close(h));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

- [ ] **Step 3: Wire the module + windows-sys features**

In `crates/vfs-shim/src/lib.rs` add alongside the other `mod` lines:

```rust
mod zipserve;
```

In `crates/vfs-shim/Cargo.toml`, ensure the `windows-sys` `features` list includes:

```toml
"Win32_Storage_FileSystem",
"Win32_System_Memory",
"Win32_System_Threading",
"Win32_Foundation",
```

- [ ] **Step 4: Run the zipserve unit test**

Run: `cargo test -p vfs-shim zipserve::`
Expected: PASS (1 test). If `MapViewOfFile` return type `.Value` does not resolve, in windows-sys 0.59 it returns `MEMORY_MAPPED_VIEW_ADDRESS { Value: *mut c_void }` — the `.Value` field access is correct.

- [ ] **Step 5: Add the Serve arm to `create_hook` and `open_hook` (hook.rs)**

In `create_hook`, add before the `Deny` arm:

```rust
        Some(Decision::Serve { container_nt, offset, length }) => {
            match crate::zipserve::open_synth(&container_nt, offset, length) {
                Some(h) => {
                    if !file_handle.is_null() {
                        *file_handle = h as HANDLE;
                    }
                    if !iosb.is_null() {
                        let p = iosb as *mut u8;
                        core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
                        core::ptr::write_unaligned(
                            p.add(8) as *mut usize,
                            crate::ntdef::FILE_OPENED,
                        );
                    }
                    STATUS_SUCCESS
                }
                // Mapping failed: fall back to the real open (likely NOT_FOUND).
                None => tramp(
                    file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen,
                ),
            }
        }
```

In `open_hook`, add the analogous arm before its `Deny` arm:

```rust
        Some(Decision::Serve { container_nt, offset, length }) => {
            match crate::zipserve::open_synth(&container_nt, offset, length) {
                Some(h) => {
                    if !file_handle.is_null() {
                        *file_handle = h as HANDLE;
                    }
                    if !iosb.is_null() {
                        let p = iosb as *mut u8;
                        core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
                        core::ptr::write_unaligned(
                            p.add(8) as *mut usize,
                            crate::ntdef::FILE_OPENED,
                        );
                    }
                    STATUS_SUCCESS
                }
                None => tramp(file_handle, access, oa, iosb, share, opts),
            }
        }
```

- [ ] **Step 6: Add the NtReadFile hook + detour**

Add the trampoline static near the others (after `TRAMP_SETINFO`):

```rust
static mut TRAMP_READ: Option<crate::ntdef::NtReadFileFn> = None;
```

Add the import to the `ntdef` `use` block: `NtReadFileFn, FileStandardInformation, FilePositionInformation, FILE_STANDARD_INFORMATION, FILE_POSITION_INFORMATION, STATUS_END_OF_FILE, STATUS_NOT_IMPLEMENTED`.

Add the hook function:

```rust
/// `NtReadFile` hook. For synthetic (zip-window) handles, copy bytes from the
/// mapped window; real handles pass straight through. `ByteOffset` of NULL or
/// the "use current position" sentinel (-1/-2) means "current position".
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn read_hook(
    handle: HANDLE,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    buffer: *mut c_void,
    length: u32,
    byte_offset: *const i64,
    key: *const u32,
) -> NTSTATUS {
    let tramp = match TRAMP_READ {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::zipserve::is_synth(handle as isize) {
        // Resolve an explicit offset only if it is a real, non-sentinel value.
        let explicit = if byte_offset.is_null() {
            None
        } else {
            let v = core::ptr::read_unaligned(byte_offset);
            if v < 0 {
                None // FILE_USE_FILE_POINTER_POSITION and friends
            } else {
                Some(v as u64)
            }
        };
        match crate::zipserve::read(handle as isize, length as usize, explicit) {
            Some((bytes, _new_pos, at_eof)) => {
                if !buffer.is_null() && !bytes.is_empty() {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        buffer as *mut u8,
                        bytes.len(),
                    );
                }
                let status = if at_eof { STATUS_END_OF_FILE } else { STATUS_SUCCESS };
                if !iosb.is_null() {
                    let p = iosb as *mut u8;
                    core::ptr::write_unaligned(p as *mut u32, status as u32);
                    core::ptr::write_unaligned(p.add(8) as *mut usize, bytes.len());
                }
                if !event.is_null() {
                    windows_sys::Win32::System::Threading::SetEvent(event);
                }
                return status;
            }
            None => return STATUS_UNSUCCESSFUL,
        }
    }
    tramp(handle, event, apc, apc_ctx, iosb, buffer, length, byte_offset, key)
}
```

In `install_all_detours`, after the `d_setinfo` block and before enabling, add:

```rust
    let d_read = make_detour(ntdll, b"NtReadFile\0", read_hook as *const ())?;
    TRAMP_READ = Some(core::mem::transmute::<*const (), crate::ntdef::NtReadFileFn>(
        d_read.trampoline() as *const (),
    ));
```

Then add `d_read.enable().map_err(|_| InstallError::Detour)?;` alongside the other enables and `detours.push(d_read);` (or extend the `detours.extend([...])` list to include `d_read`).

- [ ] **Step 7: Serve synthetic handles in qif / setinfo / close**

In `qif_hook`, at the very top after resolving `tramp`, add:

```rust
    if crate::zipserve::is_synth(handle as isize) {
        match class {
            FILE_STANDARD_INFORMATION => {
                if let Some(len) = crate::zipserve::size(handle as isize) {
                    if !info.is_null() && length as usize >= core::mem::size_of::<FileStandardInformation>() {
                        let si = info as *mut FileStandardInformation;
                        (*si).allocation_size = len as i64;
                        (*si).end_of_file = len as i64;
                        (*si).number_of_links = 1;
                        (*si).delete_pending = 0;
                        (*si).directory = 0;
                        (*si)._pad = 0;
                        if !iosb.is_null() {
                            let p = iosb as *mut u8;
                            core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
                            core::ptr::write_unaligned(
                                p.add(8) as *mut usize,
                                core::mem::size_of::<FileStandardInformation>(),
                            );
                        }
                        return STATUS_SUCCESS;
                    }
                }
                return STATUS_NOT_IMPLEMENTED;
            }
            FILE_POSITION_INFORMATION => {
                if let Some(pos) = crate::zipserve::position(handle as isize) {
                    if !info.is_null() && length as usize >= core::mem::size_of::<FilePositionInformation>() {
                        (*(info as *mut FilePositionInformation)).current_byte_offset = pos as i64;
                        return STATUS_SUCCESS;
                    }
                }
                return STATUS_NOT_IMPLEMENTED;
            }
            _ => return STATUS_NOT_IMPLEMENTED,
        }
    }
```

In `setinfo_hook`, at the top after resolving `tramp`, add:

```rust
    if crate::zipserve::is_synth(handle as isize) {
        if class == FILE_POSITION_INFORMATION
            && !info.is_null()
            && length as usize >= core::mem::size_of::<FilePositionInformation>()
        {
            let pos = (*(info as *const FilePositionInformation)).current_byte_offset;
            if pos >= 0 {
                crate::zipserve::set_position(handle as isize, pos as u64);
            }
            return STATUS_SUCCESS;
        }
        return STATUS_SUCCESS; // ignore other classes on synthetic handles
    }
```

In `close_hook`, at the top after resolving `tramp`, add:

```rust
    if crate::zipserve::is_synth(handle as isize) {
        crate::zipserve::close(handle as isize);
        return STATUS_SUCCESS;
    }
```

Ensure `FILE_POSITION_INFORMATION`, `FILE_STANDARD_INFORMATION`, `FilePositionInformation`, `FileStandardInformation`, `STATUS_NOT_IMPLEMENTED` are imported from `crate::ntdef` at the top of `hook.rs`.

- [ ] **Step 8: In-process integration test**

Create `crates/vfs-shim/tests/zip_serve_inproc.rs`:

```rust
//! In-process proof: a zip-window snapshot makes `std::fs::read` of a virtual
//! path return the exact bytes from a window inside a real container file.
use vfs_shim::install;

#[test]
fn reads_a_zip_window_through_the_hook() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-zipin-{pid}"));
    let root = base.join("gameroot");
    std::fs::create_dir_all(&root).unwrap();

    // A "container": 5 filler bytes then the payload we want to serve.
    let container = base.join("container.bin");
    let payload = b"BYTES-STRAIGHT-FROM-THE-CONTAINER";
    let mut blob = vec![b'.'; 5];
    blob.extend_from_slice(payload);
    std::fs::write(&container, &blob).unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId, SourceId};
        let src = SourceId::new(vfs_core::encode_zip_window(
            5,
            &container.to_string_lossy(),
        ));
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "asset.dat".into(),
                kind: EntryKind::File,
                source: src,
                size: payload.len() as u64,
                mtime: 1,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    let engine = vfs_shim::Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install hooks");

    let virtual_path = root.join("asset.dat");
    let got = std::fs::read(&virtual_path).expect("read virtual zip-backed file");
    assert_eq!(got, payload, "served bytes must equal the container window");
}
```

If `vfs_shim::Engine` is not re-exported, add `pub use engine::Engine;` to `crates/vfs-shim/src/lib.rs` (the existing in-process hook test already installs an engine — mirror however it obtains one).

- [ ] **Step 9: Run the shim test suite**

Run: `cargo test -p vfs-shim`
Expected: PASS — existing hook tests + `zipserve::` + the new in-process zip test. The read returns the 33-byte payload, proving open→size-query→read→EOF→close over a synthetic handle.

- [ ] **Step 10: Commit**

```bash
git add crates/vfs-shim
git commit -m "feat(vfs-shim): synthetic-handle serving of zip-window sources via NtReadFile"
```

---

### Task 5: Cross-process automated proof

**Files:**
- Create: `crates/vfs-inject/tests/zip_serve.rs`
- Reuse: `crates/vfs-inject/tests/common/` (the `locate_shim_and_payload` helper) and the `vfs-probe` bin.

**Interfaces:**
- Consumes: `vfs_inject::{run_target_with_shim, RunConfig}`, `vfs_zip::read_layer`, `vfs_shim::encode_config`.
- Add `vfs-zip = { path = "../vfs-zip" }` to `crates/vfs-inject/Cargo.toml` `[dev-dependencies]`.

- [ ] **Step 1: Write the acceptance test**

Create `crates/vfs-inject/tests/zip_serve.rs`:

```rust
//! Cross-process proof: an injected shim serves a target's read of a virtual
//! path DIRECTLY from a window inside a real Stored zip — nothing extracted.
mod common;

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use vfs_inject::{run_target_with_shim, RunConfig};

/// Write a minimal single-entry Stored zip and return (path, entry_name, content).
fn write_stored_zip(dir: &Path) -> std::path::PathBuf {
    // Reuse the same layout as vfs-zip's fixture writer.
    let name = "Data/proof.dat";
    let content = b"THESE-BYTES-LIVE-ONLY-INSIDE-THE-ZIP";
    let path = dir.join("mod.zip");
    let mut buf = Vec::new();
    let crc = crc32(content);
    let n = name.len() as u16;
    buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(name.as_bytes());
    let cd_off = buf.len() as u32;
    buf.extend_from_slice(content);
    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&0u32.to_le_bytes()); // local header offset
    buf.extend_from_slice(name.as_bytes());
    let cd_size = buf.len() as u32 - cd_start;
    buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&cd_size.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    let _ = cd_off;
    std::fs::File::create(&path).unwrap().write_all(&buf).unwrap();
    path
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[test]
fn injected_shim_serves_a_file_from_inside_a_zip() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-zipe2e-{pid}"));
    let root = base.join("gameroot");
    std::fs::create_dir_all(&root).unwrap();

    let zip = write_stored_zip(&base);
    let expected = b"THESE-BYTES-LIVE-ONLY-INSIDE-THE-ZIP".to_vec();

    // Build the snapshot straight from the zip via vfs-zip.
    let layer = vfs_zip::read_layer(&zip, vfs_core::LayerId(0)).expect("read_layer");
    let tree = vfs_core::build(vec![layer]).unwrap();
    let snapshot = vfs_shared::bridge::flatten(&tree);

    let config_bytes = vfs_shim::encode_config(root.to_str().unwrap(), &snapshot);
    let config_path = base.join("shim.cfg");
    std::fs::write(&config_path, &config_bytes).unwrap();

    let ready_path = base.join("ready.flag");
    let _ = std::fs::remove_file(&ready_path);
    let output_path = base.join("probe-out.bin");
    let _ = std::fs::remove_file(&output_path);

    // The probe opens the VIRTUAL path (root/Data/proof.dat) and copies it out.
    let virtual_path = root.join("Data").join("proof.dat");
    let probe = env!("CARGO_BIN_EXE_vfs-probe").to_string();
    let (dll, payload) = common::locate_shim_and_payload();

    let exit = run_target_with_shim(RunConfig {
        target_exe: probe,
        args: vec![
            virtual_path.to_str().unwrap().to_string(),
            output_path.to_str().unwrap().to_string(),
        ],
        dll_path: dll,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready_path.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(10),
        payload_path: payload,
        preinit_redirects: vec![],
    })
    .expect("run_target_with_shim");

    assert_eq!(exit, 0, "probe exit code");
    let got = std::fs::read(&output_path).expect("probe output");
    assert_eq!(got, expected, "served bytes must equal the zip entry content");

    // Prove nothing was extracted: the only files under `base` are our inputs.
    let extracted = std::fs::read_dir(&base)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().contains("proof.dat"));
    assert!(!extracted, "no entry should have been extracted to disk");
}
```

- [ ] **Step 2: Run the acceptance test**

Run: `cargo test -p vfs-inject --test zip_serve`
Expected: PASS — the injected shim opens the virtual path, `open_synth` maps the zip, `NtReadFile` serves the 36-byte window, and the probe writes back the exact entry bytes with nothing extracted.

- [ ] **Step 3: Full workspace regression**

Run: `cargo test --workspace`
Expected: PASS — all nine+two crates green; disk-backed redirect e2e/acceptance still pass unchanged.

- [ ] **Step 4: Commit**

```bash
git add crates/vfs-inject/tests/zip_serve.rs crates/vfs-inject/Cargo.toml
git commit -m "test(vfs-inject): cross-process proof serving a file from inside a Stored zip"
```

---

## Verification checklist (end of plan)

```
cargo test -p vfs-core source::
cargo test -p vfs-zip
cargo test -p vfs-redirect
cargo test -p vfs-shim
cargo test -p vfs-inject --test zip_serve
cargo test --workspace
```

Expect:

- [ ] `vfs-core::source` round-trips zip-window and disk blobs.
- [ ] `vfs-zip` reads a Stored entry's true data offset (fixture + real SkyUI); rejects Deflate; ZIP64 base archive readable via the `#[ignore]`d test.
- [ ] `vfs-redirect` emits `Decision::Serve` for zip-window sources; disk sources unchanged.
- [ ] `vfs-shim` serves a zip window through synthetic handles (`std::fs::read` returns exact bytes in-process).
- [ ] Cross-process: injected shim serves a file straight from a Stored zip, byte-for-byte, nothing extracted.
- [ ] `cargo test --workspace` green.

## Deferred (not in this plan)

- Memory-mapped consumers (`NtCreateSection`/`NtMapViewOfSection` on synthetic handles).
- Launching an executable that lives inside a zip (materialize only the bootstrap exe).
- Deflated-entry decompression.
- Real Skyrim launch (the eventual milestone — separate plan once the automated proof lands).

## Risk notes

- **ZIP64 offset math** is the fiddliest part; the `#[ignore]`d base-archive test validates a >4 GB offset against a known entry size.
- **`NtReadFile` completion semantics**: synchronous callers rely on the return status + IoStatusBlock. The hook signals `event` when non-null (best-effort) and always writes the IoStatusBlock. `std::fs::read` uses synchronous reads with NULL event, which is the tested path.
- **Synthetic handle detection** uses the 2^46 tag bit; real kernel handles never reach that magnitude and the sign bit stays clear.
- **windows-sys API shapes** (`MapViewOfFile` returning `MEMORY_MAPPED_VIEW_ADDRESS`, `CreateFileW` GENERIC_READ constant) are 0.59-specific — verify against the version already pinned in the workspace lockfile.
```
