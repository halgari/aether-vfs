//! Block encoding: hashing, checksums, compression and the on-disk record header.

use std::cell::RefCell;
use std::io;

/// Record marker, "SBLK" in little-endian byte order.
pub const MAGIC: u32 = u32::from_le_bytes(*b"SBLK");
/// Size of the fixed record header in bytes.
pub const HEADER_LEN: usize = 40;
/// Header flag: payload is a zstd frame (otherwise raw bytes).
pub const FLAG_COMPRESSED: u8 = 1;

/// BLAKE3 hash truncated to 128 bits; the dedup key.
pub type Hash128 = [u8; 16];

pub fn hash128(data: &[u8]) -> Hash128 {
    let mut out = [0u8; 16];
    out.copy_from_slice(&blake3::hash(data).as_bytes()[..16]);
    out
}

/// Integrity checksum over stored (possibly compressed) bytes.
pub fn checksum(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordHeader {
    pub flags: u8,
    pub raw_len: u32,
    pub stored_len: u32,
    pub hash: Hash128,
    pub checksum: u64,
}

impl RecordHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4] = self.flags;
        // b[5..8] reserved, zero
        b[8..12].copy_from_slice(&self.raw_len.to_le_bytes());
        b[12..16].copy_from_slice(&self.stored_len.to_le_bytes());
        b[16..32].copy_from_slice(&self.hash);
        b[32..40].copy_from_slice(&self.checksum.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Result<Self, &'static str> {
        if b.len() < HEADER_LEN {
            return Err("short header");
        }
        if u32::from_le_bytes(b[0..4].try_into().unwrap()) != MAGIC {
            return Err("bad magic");
        }
        let flags = b[4];
        if flags & !FLAG_COMPRESSED != 0 || b[5..8] != [0, 0, 0] {
            return Err("unknown flags");
        }
        Ok(Self {
            flags,
            raw_len: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            stored_len: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            hash: b[16..32].try_into().unwrap(),
            checksum: u64::from_le_bytes(b[32..40].try_into().unwrap()),
        })
    }

    /// Total bytes the record occupies in a pack (header + payload).
    pub fn record_len(&self) -> u64 {
        HEADER_LEN as u64 + self.stored_len as u64
    }
}

/// A block ready to be appended to a pack.
#[derive(Debug, Clone)]
pub struct EncodedBlock {
    pub header: RecordHeader,
    pub payload: Vec<u8>,
}

thread_local! {
    static COMPRESSOR: RefCell<Option<(i32, zstd::bulk::Compressor<'static>)>> =
        const { RefCell::new(None) };
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> =
        const { RefCell::new(None) };
}

/// Compresses `data` (falling back to raw if zstd does not shrink it) and builds its header.
pub fn encode_block(data: &[u8], hash: Hash128, level: i32) -> io::Result<EncodedBlock> {
    let compressed = COMPRESSOR.with(|c| -> io::Result<Vec<u8>> {
        let mut c = c.borrow_mut();
        if c.as_ref().map(|(l, _)| *l) != Some(level) {
            *c = Some((level, zstd::bulk::Compressor::new(level)?));
        }
        c.as_mut().unwrap().1.compress(data)
    })?;
    let (flags, payload) = if compressed.len() < data.len() {
        (FLAG_COMPRESSED, compressed)
    } else {
        (0, data.to_vec())
    };
    Ok(EncodedBlock {
        header: RecordHeader {
            flags,
            raw_len: data.len() as u32,
            stored_len: payload.len() as u32,
            hash,
            checksum: checksum(&payload),
        },
        payload,
    })
}

/// Verifies `payload` against `header` and writes the uncompressed block into `out`.
/// `out.len()` must equal `header.raw_len`.
pub fn decode_payload(
    header: &RecordHeader,
    payload: &[u8],
    out: &mut [u8],
) -> Result<(), &'static str> {
    if payload.len() != header.stored_len as usize {
        return Err("payload length mismatch");
    }
    if out.len() != header.raw_len as usize {
        return Err("output length mismatch");
    }
    if checksum(payload) != header.checksum {
        return Err("checksum mismatch");
    }
    if header.flags & FLAG_COMPRESSED != 0 {
        let n = DECOMPRESSOR
            .with(|d| -> io::Result<usize> {
                let mut d = d.borrow_mut();
                if d.is_none() {
                    *d = Some(zstd::bulk::Decompressor::new()?);
                }
                d.as_mut().unwrap().decompress_to_buffer(payload, out)
            })
            .map_err(|_| "decompression failed")?;
        if n != out.len() {
            return Err("decompressed length mismatch");
        }
    } else {
        out.copy_from_slice(payload);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) -> EncodedBlock {
        let enc = encode_block(data, hash128(data), 6).unwrap();
        let mut out = vec![0u8; data.len()];
        decode_payload(&enc.header, &enc.payload, &mut out).unwrap();
        assert_eq!(out, data);
        enc
    }

    #[test]
    fn compressible_block_is_compressed() {
        let enc = roundtrip(&vec![7u8; 65536]);
        assert_eq!(enc.header.flags, FLAG_COMPRESSED);
        assert!(enc.payload.len() < 1000);
    }

    #[test]
    fn incompressible_block_is_stored_raw() {
        let mut buf = vec![0u8; 65536];
        blake3::Hasher::new()
            .update(b"seed")
            .finalize_xof()
            .fill(&mut buf);
        let enc = roundtrip(&buf);
        assert_eq!(enc.header.flags, 0);
        assert_eq!(enc.payload, buf);
    }

    #[test]
    fn short_block_roundtrips() {
        roundtrip(b"hello world");
        roundtrip(b"");
    }

    #[test]
    fn header_roundtrips() {
        let h = RecordHeader {
            flags: 1,
            raw_len: 65536,
            stored_len: 123,
            hash: [9; 16],
            checksum: 42,
        };
        let bytes = h.encode();
        assert_eq!(RecordHeader::decode(&bytes).unwrap(), h);
        assert_eq!(h.record_len(), 40 + 123);
    }

    #[test]
    fn header_rejects_bad_magic_and_flags() {
        let h = RecordHeader {
            flags: 0,
            raw_len: 1,
            stored_len: 1,
            hash: [0; 16],
            checksum: 0,
        };
        let mut bytes = h.encode();
        bytes[0] ^= 0xff;
        assert_eq!(RecordHeader::decode(&bytes), Err("bad magic"));
        let mut bytes = h.encode();
        bytes[4] = 0x80;
        assert_eq!(RecordHeader::decode(&bytes), Err("unknown flags"));
        assert_eq!(RecordHeader::decode(&bytes[..10]), Err("short header"));
    }

    #[test]
    fn corrupted_payload_is_rejected() {
        let data = vec![3u8; 4096];
        let mut enc = encode_block(&data, hash128(&data), 6).unwrap();
        enc.payload[0] ^= 1;
        let mut out = vec![0u8; data.len()];
        assert_eq!(
            decode_payload(&enc.header, &enc.payload, &mut out),
            Err("checksum mismatch")
        );
    }

    #[test]
    fn hash_is_first_16_bytes_of_blake3() {
        assert_eq!(&hash128(b"abc")[..], &blake3::hash(b"abc").as_bytes()[..16]);
    }
}
