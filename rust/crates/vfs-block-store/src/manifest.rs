//! File manifest encoding. A file's manifest is split into segments of
//! `BLOCKS_PER_SEGMENT` block ids. Segment 0 starts with a `FILE_HEADER_LEN` header.
//! A block id of 0 means "not cached".

/// Block ids per manifest segment (4096 blocks = 256 MiB of data at 64 KiB blocks).
pub const BLOCKS_PER_SEGMENT: u64 = 4096;
/// Bytes of file header at the start of segment 0: len u64, flags u32, reserved u32.
pub const FILE_HEADER_LEN: usize = 16;
/// Block id meaning "not cached".
pub const MISSING: u64 = 0;

pub fn block_count(len: u64, block_size: u32) -> u64 {
    len.div_ceil(block_size as u64)
}

/// True if a file of `len` bytes has a segment count that fits in a u32 (segment keys are u32).
pub fn len_fits(len: u64, block_size: u32) -> bool {
    block_count(len, block_size).div_ceil(BLOCKS_PER_SEGMENT) <= u32::MAX as u64
}

/// Number of segments for a file with `blocks` blocks. Always at least 1 (segment 0 holds the header).
/// `blocks` must satisfy [`len_fits`].
pub fn segment_count(blocks: u64) -> u32 {
    blocks.div_ceil(BLOCKS_PER_SEGMENT).max(1) as u32
}

/// Number of block slots segment `seg` holds in a file with `blocks` blocks.
pub fn slots_in_segment(blocks: u64, seg: u32) -> usize {
    let start = seg as u64 * BLOCKS_PER_SEGMENT;
    (blocks.min(start + BLOCKS_PER_SEGMENT).saturating_sub(start)) as usize
}

/// Length in bytes of block `idx` in a file of length `len`.
pub fn block_len(len: u64, block_size: u32, idx: u64) -> u64 {
    let start = idx * block_size as u64;
    len.saturating_sub(start).min(block_size as u64)
}

pub fn encode_segment(file_len: Option<u64>, ids: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FILE_HEADER_LEN + ids.len() * 8);
    if let Some(len) = file_len {
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&[0u8; 8]);
    }
    for id in ids {
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

fn ids_bytes(value: &[u8], seg: u32) -> &[u8] {
    if seg == 0 {
        &value[FILE_HEADER_LEN..]
    } else {
        value
    }
}

/// File length stored in segment 0.
pub fn file_len(seg0: &[u8]) -> u64 {
    u64::from_le_bytes(seg0[0..8].try_into().unwrap())
}

/// Block id in slot `i` of a segment value.
pub fn slot(value: &[u8], seg: u32, i: usize) -> u64 {
    let b = ids_bytes(value, seg);
    u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap())
}

pub fn decode_ids(value: &[u8], seg: u32) -> Vec<u64> {
    let (chunks, _) = ids_bytes(value, seg).as_chunks::<8>();
    chunks.iter().map(|c| u64::from_le_bytes(*c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment0_roundtrip() {
        let v = encode_segment(Some(12345), &[1, 0, 7]);
        assert_eq!(v.len(), FILE_HEADER_LEN + 24);
        assert_eq!(file_len(&v), 12345);
        assert_eq!(slot(&v, 0, 2), 7);
        assert_eq!(decode_ids(&v, 0), vec![1, 0, 7]);
    }

    #[test]
    fn later_segment_has_no_header() {
        let v = encode_segment(None, &[5, 6]);
        assert_eq!(v.len(), 16);
        assert_eq!(slot(&v, 1, 0), 5);
        assert_eq!(decode_ids(&v, 1), vec![5, 6]);
    }

    #[test]
    fn counts_and_lengths() {
        let bs = 65536;
        assert_eq!(block_count(0, bs), 0);
        assert_eq!(block_count(1, bs), 1);
        assert_eq!(block_count(65536, bs), 1);
        assert_eq!(block_count(65537, bs), 2);
        assert_eq!(segment_count(0), 1);
        assert_eq!(segment_count(4096), 1);
        assert_eq!(segment_count(4097), 2);
        assert_eq!(block_len(65537, bs, 0), 65536);
        assert_eq!(block_len(65537, bs, 1), 1);
        assert_eq!(block_len(65537, bs, 2), 0);
        assert_eq!(slots_in_segment(0, 0), 0);
        assert_eq!(slots_in_segment(5000, 0), 4096);
        assert_eq!(slots_in_segment(5000, 1), 904);
        assert_eq!(slots_in_segment(5000, 2), 0);
    }
}
