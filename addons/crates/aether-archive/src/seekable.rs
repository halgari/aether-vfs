//! The zstd seekable format: independent zstd frames followed by a seek
//! table in a final skippable frame.
//! Spec: facebook/zstd contrib/seekable_format/zstd_seekable_compression_format.md (v0.1.0).

use std::ops::Range;

use crate::error::{Result, checksum, invalid, unsupported};
use crate::range::{RangeRead, read_vec};

const FMT: &str = "seekable zstd";

/// Magic of the skippable frame that holds the seek table.
pub const SKIPPABLE_MAGIC: u32 = 0x184D_2A5E;
/// Last four bytes of a seekable stream.
pub const SEEKABLE_MAGIC: u32 = 0x8F92_EAB1;
/// `Number_Of_Frames` (4) + `Seek_Table_Descriptor` (1) + `Seekable_Magic_Number` (4).
pub const FOOTER_LEN: usize = 9;
/// `Skippable_Magic_Number` (4) + `Frame_Size` (4).
const FRAME_HEADER_LEN: usize = 8;
const CHECKSUM_FLAG: u8 = 0x80;
const RESERVED_BITS: u8 = 0x7C;

/// Largest decompressed frame accepted (1 GiB). Real seekable streams use
/// small frames (Nexus: 4 MiB); a bigger claim is corrupt or hostile and
/// would otherwise make a reader allocate up to 4 GiB.
pub const MAX_FRAME_DECOMPRESSED: u32 = 1 << 30;

/// One compressed frame and where it sits in both address spaces.
/// Offsets are relative to the start of the seekable stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameEntry {
    pub index: usize,
    pub compressed_offset: u64,
    pub compressed_size: u32,
    pub decompressed_offset: u64,
    pub decompressed_size: u32,
    /// Low 32 bits of XXH64 (seed 0) of the decompressed frame, if the table has checksums.
    pub checksum: Option<u32>,
}

impl FrameEntry {
    pub fn compressed_range(&self) -> Range<u64> {
        self.compressed_offset..self.compressed_offset + self.compressed_size as u64
    }
    pub fn decompressed_range(&self) -> Range<u64> {
        self.decompressed_offset..self.decompressed_offset + self.decompressed_size as u64
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeekTable {
    frames: Vec<FrameEntry>,
    has_checksums: bool,
    table_frame_len: u64,
}

impl SeekTable {
    /// Length of the whole seek-table skippable frame, computed from the
    /// 9-byte footer (the last 9 bytes of the stream).
    pub fn table_frame_len(footer: &[u8]) -> Result<u64> {
        if footer.len() != FOOTER_LEN {
            return Err(invalid(
                FMT,
                format!("footer must be {FOOTER_LEN} bytes, got {}", footer.len()),
            ));
        }
        let magic = u32::from_le_bytes(footer[5..9].try_into().unwrap());
        if magic != SEEKABLE_MAGIC {
            return Err(invalid(FMT, format!("bad seekable magic {magic:#010x}")));
        }
        let descriptor = footer[4];
        if descriptor & RESERVED_BITS != 0 {
            return Err(unsupported(
                FMT,
                format!("reserved descriptor bits set: {descriptor:#04x}"),
            ));
        }
        let frames = u32::from_le_bytes(footer[0..4].try_into().unwrap()) as u64;
        let entry_len = if descriptor & CHECKSUM_FLAG != 0 {
            12
        } else {
            8
        };
        Ok(FRAME_HEADER_LEN as u64 + frames * entry_len + FOOTER_LEN as u64)
    }

    /// Parse a seek table from bytes that end exactly at the end of the
    /// seekable stream and contain at least the whole seek-table frame.
    pub fn parse(tail: &[u8]) -> Result<SeekTable> {
        if tail.len() < FOOTER_LEN {
            return Err(invalid(FMT, "stream shorter than the seek table footer"));
        }
        let footer = &tail[tail.len() - FOOTER_LEN..];
        let table_len = Self::table_frame_len(footer)?;
        if (tail.len() as u64) < table_len {
            return Err(invalid(
                FMT,
                format!("need {table_len} bytes of seek table, have {}", tail.len()),
            ));
        }
        let table = &tail[tail.len() - table_len as usize..];
        let magic = u32::from_le_bytes(table[0..4].try_into().unwrap());
        if magic != SKIPPABLE_MAGIC {
            return Err(invalid(
                FMT,
                format!("bad skippable frame magic {magic:#010x}"),
            ));
        }
        let frame_size = u32::from_le_bytes(table[4..8].try_into().unwrap()) as u64;
        if frame_size != table_len - FRAME_HEADER_LEN as u64 {
            return Err(invalid(
                FMT,
                format!(
                    "Frame_Size {frame_size} disagrees with footer ({})",
                    table_len - 8
                ),
            ));
        }
        let has_checksums = footer[4] & CHECKSUM_FLAG != 0;
        let entry_len = if has_checksums { 12 } else { 8 };
        let count = u32::from_le_bytes(footer[0..4].try_into().unwrap()) as usize;
        let mut frames = Vec::with_capacity(count);
        let (mut c_off, mut d_off) = (0u64, 0u64);
        for (index, e) in table[FRAME_HEADER_LEN..FRAME_HEADER_LEN + count * entry_len]
            .chunks_exact(entry_len)
            .enumerate()
        {
            let compressed_size = u32::from_le_bytes(e[0..4].try_into().unwrap());
            let decompressed_size = u32::from_le_bytes(e[4..8].try_into().unwrap());
            let checksum = has_checksums.then(|| u32::from_le_bytes(e[8..12].try_into().unwrap()));
            if decompressed_size > MAX_FRAME_DECOMPRESSED {
                return Err(unsupported(
                    FMT,
                    format!(
                        "frame {index} decompresses to {decompressed_size} bytes (limit {MAX_FRAME_DECOMPRESSED})"
                    ),
                ));
            }
            frames.push(FrameEntry {
                index,
                compressed_offset: c_off,
                compressed_size,
                decompressed_offset: d_off,
                decompressed_size,
                checksum,
            });
            c_off += compressed_size as u64;
            d_off += decompressed_size as u64;
        }
        Ok(SeekTable {
            frames,
            has_checksums,
            table_frame_len: table_len,
        })
    }

    /// A table for a stream that is one plain zstd frame with no seek table
    /// (how Nexus stores entries of 4 MiB or less). The frame has no table
    /// checksum; zstd's own content checksum still protects it.
    pub fn single_frame(compressed_size: u64, decompressed_size: u64) -> Result<SeekTable> {
        let c = u32::try_from(compressed_size).map_err(|_| {
            invalid(
                FMT,
                format!("single frame of {compressed_size} compressed bytes"),
            )
        })?;
        let d = u32::try_from(decompressed_size)
            .ok()
            .filter(|&d| d <= MAX_FRAME_DECOMPRESSED)
            .ok_or_else(|| {
                unsupported(
                    FMT,
                    format!("single frame of {decompressed_size} decompressed bytes"),
                )
            })?;
        Ok(SeekTable {
            frames: vec![FrameEntry {
                index: 0,
                compressed_offset: 0,
                compressed_size: c,
                decompressed_offset: 0,
                decompressed_size: d,
                checksum: None,
            }],
            has_checksums: false,
            table_frame_len: 0,
        })
    }

    /// Read and parse the seek table at the end of `r`, and check that the
    /// frames plus the table account for every byte of `r`.
    pub fn read_from<R: RangeRead + ?Sized>(r: &R) -> Result<SeekTable> {
        let len = r.len();
        if len < FOOTER_LEN as u64 {
            return Err(invalid(FMT, "stream shorter than the seek table footer"));
        }
        let footer = read_vec(r, len - FOOTER_LEN as u64, FOOTER_LEN as u64)?;
        let table_len = Self::table_frame_len(&footer)?;
        if table_len > len {
            return Err(invalid(
                FMT,
                format!("seek table ({table_len} bytes) longer than stream ({len})"),
            ));
        }
        let tail = read_vec(r, len - table_len, table_len)?;
        let table = Self::parse(&tail)?;
        if table.compressed_len() + table_len != len {
            return Err(invalid(
                FMT,
                format!(
                    "frames ({}) + seek table ({table_len}) != stream length ({len})",
                    table.compressed_len()
                ),
            ));
        }
        Ok(table)
    }

    pub fn frames(&self) -> &[FrameEntry] {
        &self.frames
    }

    pub fn has_checksums(&self) -> bool {
        self.has_checksums
    }

    /// Length of the seek-table skippable frame itself.
    pub fn table_len(&self) -> u64 {
        self.table_frame_len
    }

    /// Total compressed size of all frames (excluding the seek table).
    pub fn compressed_len(&self) -> u64 {
        self.frames
            .last()
            .map_or(0, |f| f.compressed_offset + f.compressed_size as u64)
    }

    /// Total decompressed size.
    pub fn decompressed_len(&self) -> u64 {
        self.frames
            .last()
            .map_or(0, |f| f.decompressed_offset + f.decompressed_size as u64)
    }

    /// The frames whose decompressed bytes intersect `range`, in order.
    /// Frames with no decompressed bytes are never returned.
    pub fn frames_for_range(&self, range: Range<u64>) -> Result<Vec<FrameEntry>> {
        if range.start > range.end || range.end > self.decompressed_len() {
            return Err(invalid(
                FMT,
                format!(
                    "range {range:?} outside decompressed length {}",
                    self.decompressed_len()
                ),
            ));
        }
        if range.start == range.end {
            return Ok(Vec::new());
        }
        let first = self
            .frames
            .partition_point(|f| f.decompressed_offset + f.decompressed_size as u64 <= range.start);
        let last = self
            .frames
            .partition_point(|f| f.decompressed_offset < range.end);
        Ok(self.frames[first..last]
            .iter()
            .filter(|f| f.decompressed_size > 0)
            .cloned()
            .collect())
    }
}

/// Decompress one frame into `out`, which must be exactly the frame's
/// decompressed size. `compressed` must be exactly the frame's bytes.
/// Checks the decompressed size and, if present, the checksum.
pub fn decompress_frame_into(frame: &FrameEntry, compressed: &[u8], out: &mut [u8]) -> Result<()> {
    if compressed.len() as u64 != frame.compressed_size as u64 {
        return Err(invalid(
            FMT,
            format!(
                "frame {}: got {} compressed bytes, table says {}",
                frame.index,
                compressed.len(),
                frame.compressed_size
            ),
        ));
    }
    if out.len() as u64 != frame.decompressed_size as u64 {
        return Err(invalid(
            FMT,
            format!(
                "frame {}: output buffer is {} bytes, table says {}",
                frame.index,
                out.len(),
                frame.decompressed_size
            ),
        ));
    }
    let n = zstd::bulk::Decompressor::new()?
        .decompress_to_buffer(compressed, out)
        .map_err(|e| invalid(FMT, format!("frame {}: {e}", frame.index)))?;
    if n != out.len() {
        return Err(invalid(
            FMT,
            format!(
                "frame {}: decompressed {n} bytes, table says {}",
                frame.index, frame.decompressed_size
            ),
        ));
    }
    if let Some(want) = frame.checksum {
        let got = xxhash_rust::xxh64::xxh64(out, 0) as u32;
        if got != want {
            return Err(checksum(
                FMT,
                format!(
                    "frame {}: xxh64 low32 {got:#010x} != {want:#010x}",
                    frame.index
                ),
            ));
        }
    }
    Ok(())
}

/// Decompress one frame. `compressed` must be exactly the frame's bytes.
/// Checks the decompressed size and, if present, the checksum.
pub fn decompress_frame(frame: &FrameEntry, compressed: &[u8]) -> Result<Vec<u8>> {
    if frame.decompressed_size > MAX_FRAME_DECOMPRESSED {
        return Err(unsupported(
            FMT,
            format!(
                "frame {} decompresses to {} bytes (limit {MAX_FRAME_DECOMPRESSED})",
                frame.index, frame.decompressed_size
            ),
        ));
    }
    let mut out = vec![0u8; frame.decompressed_size as usize];
    decompress_frame_into(frame, compressed, &mut out)?;
    Ok(out)
}

/// Decompress the bytes `range` of the seekable stream `r`.
pub fn read_range<R: RangeRead + ?Sized>(
    r: &R,
    table: &SeekTable,
    range: Range<u64>,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity((range.end.saturating_sub(range.start)) as usize);
    for frame in table.frames_for_range(range.clone())? {
        let compressed = read_vec(r, frame.compressed_offset, frame.compressed_size as u64)?;
        let data = decompress_frame(&frame, &compressed)?;
        let d = frame.decompressed_range();
        let from = range.start.max(d.start) - d.start;
        let to = range.end.min(d.end) - d.start;
        out.extend_from_slice(&data[from as usize..to as usize]);
    }
    Ok(out)
}
