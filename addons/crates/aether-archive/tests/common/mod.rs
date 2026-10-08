//! Shared test fixtures: deterministic data, seekable zstd and hand-built zips.
#![allow(dead_code)]

/// Compressible bytes (same formula as the K4os golden vectors).
pub fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i % 251) ^ (i / 1000)) as u8).collect()
}

/// Incompressible bytes (64-bit LCG, top byte).
pub fn lcg(n: usize) -> Vec<u8> {
    let mut x: u64 = 1;
    (0..n)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 56) as u8
        })
        .collect()
}

/// Encode `data` as seekable zstd: independent frames of `frame_size`
/// bytes, then the seek table skippable frame.
pub fn seekable_zstd(data: &[u8], frame_size: usize, checksums: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut entries = Vec::new();
    for chunk in data.chunks(frame_size) {
        let c = zstd::bulk::compress(chunk, 3).unwrap();
        entries.extend_from_slice(&(c.len() as u32).to_le_bytes());
        entries.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        if checksums {
            entries.extend_from_slice(&(xxhash_rust::xxh64::xxh64(chunk, 0) as u32).to_le_bytes());
        }
        out.extend_from_slice(&c);
    }
    let frames = data.chunks(frame_size).count() as u32;
    let frame_size_field = entries.len() as u32 + 9;
    out.extend_from_slice(&0x184D_2A5Eu32.to_le_bytes());
    out.extend_from_slice(&frame_size_field.to_le_bytes());
    out.extend_from_slice(&entries);
    out.extend_from_slice(&frames.to_le_bytes());
    out.push(if checksums { 0x80 } else { 0 });
    out.extend_from_slice(&0x8F92_EAB1u32.to_le_bytes());
    out
}

pub struct TestEntry {
    pub name: Vec<u8>,
    pub method: u16,
    pub flags: u16,
    /// Bytes stored in the zip (already compressed for non-stored methods).
    pub stored: Vec<u8>,
    pub uncompressed_size: u64,
    /// Extra field bytes written only in the central directory record (after
    /// the ZIP64 field), like Nexus's `0x4E58` seek-table copy.
    pub central_extra: Vec<u8>,
}

impl TestEntry {
    pub fn stored(name: &str, data: &[u8]) -> Self {
        TestEntry {
            name: name.as_bytes().to_vec(),
            method: 0,
            flags: 0,
            stored: data.to_vec(),
            uncompressed_size: data.len() as u64,
            central_extra: Vec::new(),
        }
    }
    pub fn seekable(name: &str, data: &[u8], frame_size: usize) -> Self {
        TestEntry {
            name: name.as_bytes().to_vec(),
            method: 93,
            flags: 0,
            stored: seekable_zstd(data, frame_size, true),
            uncompressed_size: data.len() as u64,
            central_extra: Vec::new(),
        }
    }

    /// A seekable entry that also carries its seek table in a Nexus
    /// `0x4E58` central-directory extra field.
    pub fn nexus(name: &str, data: &[u8], frame_size: usize) -> Self {
        let mut e = TestEntry::seekable(name, data, frame_size);
        let footer = &e.stored[e.stored.len() - 9..];
        let frames = u32::from_le_bytes(footer[0..4].try_into().unwrap()) as usize;
        let table_len = 8 + frames * 12 + 9;
        let table = e.stored[e.stored.len() - table_len..].to_vec();
        e.central_extra = nexus_extra(&table);
        e
    }
}

/// A `0x4E58` extra field (id, size, payload) wrapping `table`.
pub fn nexus_extra(table: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&0x4E58u16.to_le_bytes());
    v.extend_from_slice(&(table.len() as u16).to_le_bytes());
    v.extend_from_slice(table);
    v
}

fn p16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn p32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn p64(v: &mut Vec<u8>, x: u64) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// Build a zip by hand. `prefix` bytes are written first (like a
/// self-extractor stub). With `zip64`, every size/offset goes through the
/// ZIP64 extra field and a ZIP64 end record + locator are written. CRCs are 0
/// (the reader does not check them).
pub fn build_zip(prefix: &[u8], entries: &[TestEntry], zip64: bool, comment: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    let mut offsets = Vec::new();
    for e in entries {
        offsets.push(out.len() as u64);
        p32(&mut out, 0x0403_4b50);
        p16(&mut out, if zip64 { 45 } else { 20 });
        p16(&mut out, e.flags);
        p16(&mut out, e.method);
        p16(&mut out, 0);
        p16(&mut out, 0);
        p32(&mut out, 0);
        if zip64 {
            p32(&mut out, 0xFFFF_FFFF);
            p32(&mut out, 0xFFFF_FFFF);
        } else {
            p32(&mut out, e.stored.len() as u32);
            p32(&mut out, e.uncompressed_size as u32);
        }
        p16(&mut out, e.name.len() as u16);
        p16(&mut out, if zip64 { 20 } else { 0 });
        out.extend_from_slice(&e.name);
        if zip64 {
            p16(&mut out, 1);
            p16(&mut out, 16);
            p64(&mut out, e.uncompressed_size);
            p64(&mut out, e.stored.len() as u64);
        }
        out.extend_from_slice(&e.stored);
    }
    let cd_start = out.len() as u64;
    for (e, &off) in entries.iter().zip(&offsets) {
        p32(&mut out, 0x0201_4b50);
        p16(&mut out, 45);
        p16(&mut out, if zip64 { 45 } else { 20 });
        p16(&mut out, e.flags);
        p16(&mut out, e.method);
        p16(&mut out, 0);
        p16(&mut out, 0);
        p32(&mut out, 0);
        if zip64 {
            p32(&mut out, 0xFFFF_FFFF);
            p32(&mut out, 0xFFFF_FFFF);
        } else {
            p32(&mut out, e.stored.len() as u32);
            p32(&mut out, e.uncompressed_size as u32);
        }
        p16(&mut out, e.name.len() as u16);
        p16(
            &mut out,
            if zip64 { 28 } else { 0 } + e.central_extra.len() as u16,
        );
        p16(&mut out, 0);
        p16(&mut out, 0);
        p16(&mut out, 0);
        p32(&mut out, 0);
        p32(&mut out, if zip64 { 0xFFFF_FFFF } else { off as u32 });
        out.extend_from_slice(&e.name);
        if zip64 {
            p16(&mut out, 1);
            p16(&mut out, 24);
            p64(&mut out, e.uncompressed_size);
            p64(&mut out, e.stored.len() as u64);
            p64(&mut out, off);
        }
        out.extend_from_slice(&e.central_extra);
    }
    let cd_size = out.len() as u64 - cd_start;
    if zip64 {
        let z64_pos = out.len() as u64;
        p32(&mut out, 0x0606_4b50);
        p64(&mut out, 44);
        p16(&mut out, 45);
        p16(&mut out, 45);
        p32(&mut out, 0);
        p32(&mut out, 0);
        p64(&mut out, entries.len() as u64);
        p64(&mut out, entries.len() as u64);
        p64(&mut out, cd_size);
        p64(&mut out, cd_start);
        p32(&mut out, 0x0706_4b50);
        p32(&mut out, 0);
        p64(&mut out, z64_pos);
        p32(&mut out, 1);
    }
    p32(&mut out, 0x0605_4b50);
    p16(&mut out, 0);
    p16(&mut out, 0);
    let n = if zip64 { 0xFFFF } else { entries.len() as u16 };
    p16(&mut out, n);
    p16(&mut out, n);
    p32(&mut out, if zip64 { 0xFFFF_FFFF } else { cd_size as u32 });
    p32(&mut out, if zip64 { 0xFFFF_FFFF } else { cd_start as u32 });
    p16(&mut out, comment.len() as u16);
    out.extend_from_slice(comment);
    out
}
