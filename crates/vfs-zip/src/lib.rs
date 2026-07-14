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
    // A well-formed zip must be at least large enough to hold an EOCD record.
    if file_len < 22 {
        return Err(ZipError::NotAZip);
    }
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
    // `count` is untrusted (read straight off disk); cap eager preallocation so a
    // corrupt archive claiming a huge count can't abort via allocation failure.
    // The read_exact calls below will still surface a real error once data runs out.
    let mut entries = Vec::with_capacity((count as usize).min(4096));
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

    #[test]
    fn a_too_small_file_is_not_a_zip() {
        let dir = std::env::temp_dir().join(format!("vfs-zip-tiny-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.bin");
        std::fs::write(&path, b"PK").unwrap(); // 2 bytes, no EOCD
        assert!(matches!(read_layer(&path, LayerId(0)), Err(ZipError::NotAZip)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
