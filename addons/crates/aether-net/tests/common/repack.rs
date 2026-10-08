//! Build zips shaped like Nexus's repacked archives (always ZIP64,
//! data descriptors, method 93, NTFS extra, 0x4E58 in the central directory).

use std::io::Write;

pub const FRAME: usize = 4 << 20;

pub struct RepackFile {
    pub name: String,
    pub data: Vec<u8>,
    /// Decompressed frame size for entries larger than it.
    pub frame_size: usize,
    /// Copy the seek table into a 0x4E58 extra field (Nexus always does
    /// for multi-frame entries).
    pub nexus_extra: bool,
    /// Corrupt this frame's checksum in the seek table.
    pub bad_checksum_frame: Option<usize>,
    /// Lie about the entry's uncompressed size in the central directory
    /// (the local header, data descriptor and actual bytes stay real).
    pub claimed_uncompressed_size: Option<u64>,
    /// Lie about the entry's compressed size in the central directory.
    pub claimed_compressed_size: Option<u64>,
    /// Lie about the entry's local header offset in the central directory.
    pub claimed_local_header_offset: Option<u64>,
    /// Store this entry uncompressed (method 0) instead of zstd (93).
    pub force_stored: bool,
    /// Bytes of an extra padding field in the local header only (the
    /// central directory's extra length then says nothing about it).
    pub local_extra_pad: usize,
}

impl RepackFile {
    pub fn new(name: &str, data: Vec<u8>) -> RepackFile {
        RepackFile {
            name: name.into(),
            data,
            frame_size: FRAME,
            nexus_extra: true,
            bad_checksum_frame: None,
            claimed_uncompressed_size: None,
            claimed_compressed_size: None,
            claimed_local_header_offset: None,
            force_stored: false,
            local_extra_pad: 0,
        }
    }
    pub fn dir(name: &str) -> RepackFile {
        assert!(name.ends_with('/'));
        RepackFile::new(name, Vec::new())
    }
    pub fn frames(mut self, frame_size: usize) -> RepackFile {
        self.frame_size = frame_size;
        self
    }
}

/// Compressible, but not trivially (so frames have distinct sizes).
pub fn data(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|i| {
            if i % 7 == 0 {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (x >> 56) as u8
            } else {
                (i % 251) as u8
            }
        })
        .collect()
}

fn zstd_frame(chunk: &[u8]) -> Vec<u8> {
    let mut enc = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
    enc.include_checksum(true).unwrap();
    enc.set_pledged_src_size(Some(chunk.len() as u64)).unwrap();
    enc.write_all(chunk).unwrap();
    enc.finish().unwrap()
}

/// (stored bytes, seek table frame if multi-frame)
fn compress(f: &RepackFile) -> (Vec<u8>, Option<Vec<u8>>) {
    if f.data.len() <= f.frame_size {
        return (zstd_frame(&f.data), None);
    }
    let mut out = Vec::new();
    let mut entries = Vec::new();
    for (i, chunk) in f.data.chunks(f.frame_size).enumerate() {
        let c = zstd::bulk::compress(chunk, 3).unwrap();
        entries.extend_from_slice(&(c.len() as u32).to_le_bytes());
        entries.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        let mut sum = xxhash_rust::xxh64::xxh64(chunk, 0) as u32;
        if f.bad_checksum_frame == Some(i) {
            sum ^= 1;
        }
        entries.extend_from_slice(&sum.to_le_bytes());
        out.extend_from_slice(&c);
    }
    let n = f.data.chunks(f.frame_size).count() as u32;
    let mut table = Vec::new();
    table.extend_from_slice(&0x184D_2A5Eu32.to_le_bytes());
    table.extend_from_slice(&(entries.len() as u32 + 9).to_le_bytes());
    table.extend_from_slice(&entries);
    table.extend_from_slice(&n.to_le_bytes());
    table.push(0x80);
    table.extend_from_slice(&0x8F92_EAB1u32.to_le_bytes());
    out.extend_from_slice(&table);
    (out, Some(table))
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

fn ntfs_extra(v: &mut Vec<u8>) {
    p16(v, 0x000a);
    p16(v, 32);
    p32(v, 0);
    p16(v, 1);
    p16(v, 24);
    for _ in 0..3 {
        p64(v, 133_000_000_000_000_000);
    }
}

pub fn repacked_zip(files: &[RepackFile]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    for f in files {
        let dir = f.name.ends_with('/');
        let stored_method = !dir && f.force_stored;
        if !dir && !stored_method {
            assert!(
                f.nexus_extra || f.data.len() as u64 > 4 << 20,
                "Nexus puts 0x4E58 on every multi-frame entry; only >4 MiB entries may omit it here"
            );
        }
        let (stored, table) = if dir || stored_method {
            (f.data.clone(), None)
        } else {
            compress(f)
        };
        let method: u16 = if dir || stored_method { 0 } else { 93 };
        let offset = out.len() as u64;
        // Local header: sizes deferred to the data descriptor.
        p32(&mut out, 0x0403_4b50);
        p16(&mut out, 63);
        p16(&mut out, 0x0808);
        p16(&mut out, method);
        p32(&mut out, 0);
        p32(&mut out, 0);
        p32(&mut out, 0xFFFF_FFFF);
        p32(&mut out, 0xFFFF_FFFF);
        p16(&mut out, f.name.len() as u16);
        let pad = if f.local_extra_pad > 0 {
            4 + f.local_extra_pad
        } else {
            0
        };
        p16(&mut out, (20 + 36 + pad) as u16);
        out.extend_from_slice(f.name.as_bytes());
        p16(&mut out, 1);
        p16(&mut out, 16);
        p64(&mut out, 0);
        p64(&mut out, 0);
        ntfs_extra(&mut out);
        if f.local_extra_pad > 0 {
            p16(&mut out, 0xCAFE);
            p16(&mut out, f.local_extra_pad as u16);
            out.extend(std::iter::repeat_n(0u8, f.local_extra_pad));
        }
        out.extend_from_slice(&stored);
        p32(&mut out, 0x0807_4b50);
        p32(&mut out, 0);
        p64(&mut out, stored.len() as u64);
        p64(&mut out, f.data.len() as u64);
        // Central record.
        let nexus = table.filter(|_| f.nexus_extra);
        p32(&mut central, 0x0201_4b50);
        p16(&mut central, 63);
        p16(&mut central, 63);
        p16(&mut central, 0x0808);
        p16(&mut central, method);
        p32(&mut central, 0);
        p32(&mut central, 0);
        p32(&mut central, 0xFFFF_FFFF);
        p32(&mut central, 0xFFFF_FFFF);
        p16(&mut central, f.name.len() as u16);
        let extra_len = 28 + 36 + nexus.as_ref().map_or(0, |t| 4 + t.len());
        p16(&mut central, extra_len as u16);
        p16(&mut central, 0);
        p16(&mut central, 0);
        p16(&mut central, 0);
        p32(&mut central, 0);
        p32(&mut central, 0xFFFF_FFFF);
        central.extend_from_slice(f.name.as_bytes());
        p16(&mut central, 1);
        p16(&mut central, 24);
        p64(
            &mut central,
            f.claimed_uncompressed_size.unwrap_or(f.data.len() as u64),
        );
        p64(
            &mut central,
            f.claimed_compressed_size.unwrap_or(stored.len() as u64),
        );
        p64(
            &mut central,
            f.claimed_local_header_offset.unwrap_or(offset),
        );
        ntfs_extra(&mut central);
        if let Some(t) = nexus {
            p16(&mut central, 0x4E58);
            p16(&mut central, t.len() as u16);
            central.extend_from_slice(&t);
        }
    }
    let cd_start = out.len() as u64;
    out.extend_from_slice(&central);
    let z64 = out.len() as u64;
    p32(&mut out, 0x0606_4b50);
    p64(&mut out, 44);
    p16(&mut out, 63);
    p16(&mut out, 63);
    p32(&mut out, 0);
    p32(&mut out, 0);
    p64(&mut out, files.len() as u64);
    p64(&mut out, files.len() as u64);
    p64(&mut out, central.len() as u64);
    p64(&mut out, cd_start);
    p32(&mut out, 0x0706_4b50);
    p32(&mut out, 0);
    p64(&mut out, z64);
    p32(&mut out, 1);
    let comment = br#"{"version":1,"gameId":1704,"modId":1,"files":[{"fileId":1,"uuid":"test"}]}"#;
    p32(&mut out, 0x0605_4b50);
    p16(&mut out, 0);
    p16(&mut out, 0);
    p16(&mut out, files.len() as u16);
    p16(&mut out, files.len() as u16);
    p32(&mut out, central.len() as u32);
    p32(&mut out, cd_start as u32);
    p16(&mut out, comment.len() as u16);
    out.extend_from_slice(comment);
    out
}
