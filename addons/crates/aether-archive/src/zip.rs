//! Zip central directory and local header parsing over a `RangeRead`,
//! including ZIP64, plus seekable-zstd (method 93) entry access.
//! Reference: PKWARE APPNOTE.TXT 6.3.10, sections 4.3.7, 4.3.12, 4.3.14-4.3.16, 4.5.3.

use std::ops::Range;

use std::collections::HashMap;

use crate::error::{FormatError, Result, invalid, unsupported};
use crate::path::fold;
use crate::range::{RangeRead, SubRange, read_vec};
use crate::seekable::{self, FrameEntry, SeekTable};

const FMT: &str = "zip";

pub const METHOD_STORED: u16 = 0;
pub const METHOD_DEFLATE: u16 = 8;
pub const METHOD_ZSTD: u16 = 93;
/// Central-directory-only extra field in Nexus repacked zips: a verbatim copy
/// of the entry's seekable-zstd seek-table skippable frame.
pub const NEXUS_SEEK_TABLE_EXTRA_ID: u16 = 0x4E58;
/// Nexus stores entries of at most this many bytes as one plain zstd frame
/// with no seek table.
pub const PLAIN_FRAME_MAX: u64 = 4 << 20;

const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;
const SIG_ZIP64_EOCD: u32 = 0x0606_4b50;
const SIG_ZIP64_LOCATOR: u32 = 0x0706_4b50;
const EOCD_LEN: u64 = 22;
const LOCATOR_LEN: u64 = 20;
/// Fixed part of a ZIP64 end-of-central-directory record.
pub const ZIP64_EOCD_LEN: u64 = 56;
const CENTRAL_LEN: usize = 46;
/// Fixed part of a local file header.
pub const LOCAL_HEADER_LEN: u64 = 30;
const MAX_COMMENT: u64 = 0xFFFF;
/// Bytes from the end of a zip that always contain its end records: an EOCD
/// with the longest possible comment, the ZIP64 locator and a ZIP64 record
/// directly before it.
pub const MAX_TAIL: u64 = EOCD_LEN + MAX_COMMENT + LOCATOR_LEN + ZIP64_EOCD_LEN;
const ZIP64_EXTRA_ID: u16 = 0x0001;
const FLAG_ENCRYPTED: u16 = 0x0001;
const FLAG_UTF8: u16 = 0x0800;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZipEntry {
    /// Name as stored (forward slashes), decoded as UTF-8 when flagged or valid, else CP437.
    pub name: String,
    pub method: u16,
    pub flags: u16,
    pub crc32: u32,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    pub local_header_offset: u64,
    /// The seek table from a Nexus `0x4E58` extra field, already checked
    /// against this entry's sizes.
    pub seek_table: Option<SeekTable>,
}

impl ZipEntry {
    pub fn is_dir(&self) -> bool {
        self.name.ends_with('/')
    }

    /// Frame map of a zstd (method 93) entry, from the central directory
    /// alone: the Nexus seek table if present, else one plain frame for an
    /// entry of at most [`PLAIN_FRAME_MAX`] bytes. `None` means the seek
    /// table must be read from the end of the entry's data
    /// ([`SeekableEntry::open`]).
    pub fn frame_map(&self) -> Result<Option<SeekTable>> {
        if self.method != METHOD_ZSTD {
            return Err(unsupported(
                FMT,
                format!("{}: method {} is not zstd (93)", self.name, self.method),
            ));
        }
        if let Some(t) = &self.seek_table {
            return Ok(Some(t.clone()));
        }
        if self.uncompressed_size <= PLAIN_FRAME_MAX {
            return SeekTable::single_frame(self.compressed_size, self.uncompressed_size).map(Some);
        }
        Ok(None)
    }
}

/// Where a zip's central directory is, from its end records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CentralDirectory {
    pub offset: u64,
    pub size: u64,
    pub entries: u64,
}

impl CentralDirectory {
    /// Parse a ZIP64 end-of-central-directory record (at least
    /// [`ZIP64_EOCD_LEN`] bytes) of a `file_len`-byte zip.
    pub fn from_zip64_record(rec: &[u8], file_len: u64) -> Result<CentralDirectory> {
        if (rec.len() as u64) < ZIP64_EOCD_LEN || u32_at(rec, 0) != SIG_ZIP64_EOCD {
            return Err(invalid(FMT, "bad ZIP64 end-of-central-directory signature"));
        }
        if u32_at(rec, 16) != 0 || u32_at(rec, 20) != 0 {
            return Err(unsupported(FMT, "multi-disk ZIP64 archives"));
        }
        CentralDirectory {
            entries: u64_at(rec, 32),
            size: u64_at(rec, 40),
            offset: u64_at(rec, 48),
        }
        .within(file_len)
    }

    fn within(self, file_len: u64) -> Result<CentralDirectory> {
        match self.offset.checked_add(self.size) {
            Some(end) if end <= file_len => Ok(self),
            _ => Err(invalid(
                FMT,
                format!(
                    "central directory {}+{} outside {file_len}-byte file",
                    self.offset, self.size
                ),
            )),
        }
    }
}

/// Result of [`locate_central_directory`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndRecords {
    Found(CentralDirectory),
    /// The ZIP64 end record is at this absolute offset, before the tail:
    /// read [`ZIP64_EOCD_LEN`] bytes there and pass them to
    /// [`CentralDirectory::from_zip64_record`].
    NeedZip64Record(u64),
}

/// Find the central directory from the last bytes of a zip. `tail` must end
/// at end of file, start at absolute offset `tail_start`, and be at least
/// [`MAX_TAIL`] bytes long unless it is the whole file.
pub fn locate_central_directory(tail: &[u8], tail_start: u64) -> Result<EndRecords> {
    let file_len = tail_start
        .checked_add(tail.len() as u64)
        .ok_or_else(|| invalid(FMT, "tail_start + tail length overflows"))?;
    if (tail.len() as u64) < EOCD_LEN {
        return Err(invalid(
            FMT,
            "file shorter than an end-of-central-directory record",
        ));
    }
    if tail_start != 0 && (tail.len() as u64) < MAX_TAIL {
        return Err(invalid(
            FMT,
            format!("tail of {} bytes is shorter than {MAX_TAIL}", tail.len()),
        ));
    }
    let eocd_at = |exact: bool| {
        (0..=tail.len() - EOCD_LEN as usize).rev().find(|&i| {
            let end = i + EOCD_LEN as usize + u16_at(tail, i + 20) as usize;
            u32_at(tail, i) == SIG_EOCD
                && if exact {
                    end == tail.len()
                } else {
                    end <= tail.len()
                }
        })
    };
    // Normally the comment runs exactly to end of file; tolerate trailing
    // bytes after it (some writers append padding or an index).
    let eocd_rel = eocd_at(true)
        .or_else(|| eocd_at(false))
        .ok_or_else(|| invalid(FMT, "no end-of-central-directory record"))?;
    let eocd = &tail[eocd_rel..eocd_rel + EOCD_LEN as usize];
    let disk = u16_at(eocd, 4);
    let cd_disk = u16_at(eocd, 6);
    if (disk != 0 || cd_disk != 0) && disk != 0xFFFF && cd_disk != 0xFFFF {
        return Err(unsupported(FMT, "multi-disk archives"));
    }
    let classic = CentralDirectory {
        entries: u16_at(eocd, 10) as u64,
        size: u32_at(eocd, 12) as u64,
        offset: u32_at(eocd, 16) as u64,
    };
    let locator = eocd_rel
        .checked_sub(LOCATOR_LEN as usize)
        .map(|l| &tail[l..l + LOCATOR_LEN as usize])
        .filter(|l| u32_at(l, 0) == SIG_ZIP64_LOCATOR);
    let Some(loc) = locator else {
        if classic.entries == 0xFFFF || classic.size == 0xFFFF_FFFF || classic.offset == 0xFFFF_FFFF
        {
            return Err(invalid(FMT, "EOCD has ZIP64 markers but no ZIP64 locator"));
        }
        return classic.within(file_len).map(EndRecords::Found);
    };
    if u32_at(loc, 16) > 1 {
        return Err(unsupported(FMT, "multi-disk ZIP64 archives"));
    }
    let z64_pos = u64_at(loc, 8);
    match z64_pos.checked_add(ZIP64_EOCD_LEN) {
        Some(end) if end <= file_len => {}
        _ => {
            return Err(invalid(
                FMT,
                "ZIP64 end-of-central-directory record out of range",
            ));
        }
    }
    if z64_pos < tail_start {
        return Ok(EndRecords::NeedZip64Record(z64_pos));
    }
    let rel = (z64_pos - tail_start) as usize;
    CentralDirectory::from_zip64_record(&tail[rel..], file_len).map(EndRecords::Found)
}

/// The parsed central directory of a zip.
#[derive(Debug, Clone)]
pub struct ZipIndex {
    entries: Vec<ZipEntry>,
    /// Folded name -> first entry with that name.
    by_name: HashMap<String, usize>,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

impl ZipIndex {
    /// Locate the end of central directory (ZIP64 if present) and parse every entry.
    pub fn read<R: RangeRead + ?Sized>(r: &R) -> Result<ZipIndex> {
        let len = r.len();
        let tail_len = len.min(MAX_TAIL);
        let tail_start = len - tail_len;
        let tail = read_vec(r, tail_start, tail_len)?;
        let cd = match locate_central_directory(&tail, tail_start)? {
            EndRecords::Found(cd) => cd,
            EndRecords::NeedZip64Record(pos) => {
                CentralDirectory::from_zip64_record(&read_vec(r, pos, ZIP64_EOCD_LEN)?, len)?
            }
        };
        let bytes = read_vec(r, cd.offset, cd.size)?;
        ZipIndex::parse_central_directory(&bytes, cd.entries)
    }

    /// Parse `count` entries from the raw central directory bytes.
    pub fn parse_central_directory(cd: &[u8], count: u64) -> Result<ZipIndex> {
        // Every entry needs at least 46 bytes; reject absurd counts before allocating.
        if count > cd.len() as u64 / CENTRAL_LEN as u64 {
            return Err(invalid(
                FMT,
                format!(
                    "{count} entries cannot fit in {}-byte central directory",
                    cd.len()
                ),
            ));
        }
        let mut entries = Vec::with_capacity(count as usize);
        let mut p = 0usize;
        for i in 0..count {
            if p + CENTRAL_LEN > cd.len() || u32_at(cd, p) != SIG_CENTRAL {
                return Err(invalid(
                    FMT,
                    format!("central directory entry {i} is corrupt"),
                ));
            }
            let h = &cd[p..p + CENTRAL_LEN];
            let flags = u16_at(h, 8);
            let method = u16_at(h, 10);
            let crc32 = u32_at(h, 16);
            let mut compressed_size = u32_at(h, 20) as u64;
            let mut uncompressed_size = u32_at(h, 24) as u64;
            let name_len = u16_at(h, 28) as usize;
            let extra_len = u16_at(h, 30) as usize;
            let comment_len = u16_at(h, 32) as usize;
            let disk_start = u16_at(h, 34);
            let mut local_header_offset = u32_at(h, 42) as u64;
            let var_end = p + CENTRAL_LEN + name_len + extra_len + comment_len;
            if var_end > cd.len() {
                return Err(invalid(
                    FMT,
                    format!("central directory entry {i} runs past the directory"),
                ));
            }
            let name_bytes = &cd[p + CENTRAL_LEN..p + CENTRAL_LEN + name_len];
            let extra = &cd[p + CENTRAL_LEN + name_len..p + CENTRAL_LEN + name_len + extra_len];
            if uncompressed_size == 0xFFFF_FFFF
                || compressed_size == 0xFFFF_FFFF
                || local_header_offset == 0xFFFF_FFFF
                || disk_start == 0xFFFF
            {
                let z = find_extra(extra, ZIP64_EXTRA_ID)
                    .ok_or_else(|| invalid(FMT, format!("entry {i} needs a ZIP64 extra field")))?;
                let mut q = 0usize;
                let mut next = |what: &str| -> Result<u64> {
                    if q + 8 > z.len() {
                        return Err(invalid(
                            FMT,
                            format!("entry {i}: ZIP64 extra field missing {what}"),
                        ));
                    }
                    let v = u64_at(z, q);
                    q += 8;
                    Ok(v)
                };
                if uncompressed_size == 0xFFFF_FFFF {
                    uncompressed_size = next("uncompressed size")?;
                }
                if compressed_size == 0xFFFF_FFFF {
                    compressed_size = next("compressed size")?;
                }
                if local_header_offset == 0xFFFF_FFFF {
                    local_header_offset = next("local header offset")?;
                }
            }
            let name = decode_name(name_bytes, flags);
            let seek_table = match find_extra(extra, NEXUS_SEEK_TABLE_EXTRA_ID) {
                None => None,
                Some(raw) => Some(nexus_seek_table(
                    &name,
                    raw,
                    compressed_size,
                    uncompressed_size,
                )?),
            };
            entries.push(ZipEntry {
                name,
                method,
                flags,
                crc32,
                compressed_size,
                uncompressed_size,
                local_header_offset,
                seek_table,
            });
            p = var_end;
        }
        let mut by_name = HashMap::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            by_name.entry(fold(&e.name)).or_insert(i);
        }
        Ok(ZipIndex { entries, by_name })
    }

    pub fn entries(&self) -> &[ZipEntry] {
        &self.entries
    }

    /// Find an entry by path. `\` and `/` are equivalent on both sides (some
    /// Windows tools store backslashes) and case is ignored (Wabbajack paths
    /// are Windows paths); see [`crate::path::fold`].
    pub fn find(&self, path: &str) -> Option<&ZipEntry> {
        self.position(path).map(|i| &self.entries[i])
    }

    /// Index in [`ZipIndex::entries`] of the entry [`ZipIndex::find`] returns.
    pub fn position(&self, path: &str) -> Option<usize> {
        self.by_name.get(&fold(path)).copied()
    }
}

/// Parse and check a Nexus `0x4E58` extra field: it must be exactly one
/// seek-table frame describing the entry's compressed and uncompressed sizes.
fn nexus_seek_table(
    name: &str,
    raw: &[u8],
    compressed_size: u64,
    uncompressed_size: u64,
) -> Result<SeekTable> {
    let t = SeekTable::parse(raw).map_err(|e| match e {
        FormatError::Invalid { msg, .. } => {
            invalid(FMT, format!("{name}: 0x4E58 seek table: {msg}"))
        }
        other => other,
    })?;
    if t.table_len() != raw.len() as u64
        || t.decompressed_len() != uncompressed_size
        || t.compressed_len() + t.table_len() != compressed_size
    {
        return Err(invalid(
            FMT,
            format!(
                "{name}: 0x4E58 seek table ({} frames, {} -> {} bytes) does not match the entry ({compressed_size} -> {uncompressed_size})",
                t.frames().len(),
                t.compressed_len() + t.table_len(),
                t.decompressed_len()
            ),
        ));
    }
    Ok(t)
}

fn find_extra(mut extra: &[u8], id: u16) -> Option<&[u8]> {
    while extra.len() >= 4 {
        let hid = u16_at(extra, 0);
        let size = u16_at(extra, 2) as usize;
        if 4 + size > extra.len() {
            return None;
        }
        if hid == id {
            return Some(&extra[4..4 + size]);
        }
        extra = &extra[4 + size..];
    }
    None
}

const CP437_HIGH: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', //
    'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ', //
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', //
    '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐', //
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', //
    '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀', //
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', //
    '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', '\u{a0}',
];

/// UTF-8 if the language-encoding flag is set or the bytes are valid UTF-8; otherwise CP437.
fn decode_name(bytes: &[u8], flags: u16) -> String {
    if flags & FLAG_UTF8 != 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => bytes
            .iter()
            .map(|&b| {
                if b < 0x80 {
                    b as char
                } else {
                    CP437_HIGH[(b - 0x80) as usize]
                }
            })
            .collect(),
    }
}

/// Absolute byte range of an entry's (compressed) data, given at least the
/// first [`LOCAL_HEADER_LEN`] bytes of its local header (read at
/// `entry.local_header_offset`). The caller checks the range against the
/// file length.
pub fn local_data_range(local_header: &[u8], entry: &ZipEntry) -> Result<Range<u64>> {
    if entry.flags & FLAG_ENCRYPTED != 0 {
        return Err(unsupported(
            FMT,
            format!("{}: encrypted entries", entry.name),
        ));
    }
    if (local_header.len() as u64) < LOCAL_HEADER_LEN || u32_at(local_header, 0) != SIG_LOCAL {
        return Err(invalid(
            FMT,
            format!("{}: bad local header signature", entry.name),
        ));
    }
    let start = entry
        .local_header_offset
        .checked_add(LOCAL_HEADER_LEN)
        .and_then(|v| v.checked_add(u16_at(local_header, 26) as u64))
        .and_then(|v| v.checked_add(u16_at(local_header, 28) as u64))
        .ok_or_else(|| {
            invalid(
                FMT,
                format!("{}: local header offset overflows", entry.name),
            )
        })?;
    let end = start
        .checked_add(entry.compressed_size)
        .ok_or_else(|| invalid(FMT, format!("{}: data size overflows", entry.name)))?;
    Ok(start..end)
}

/// Absolute byte range of an entry's (compressed) data, from its local header.
pub fn entry_data_range<R: RangeRead + ?Sized>(r: &R, entry: &ZipEntry) -> Result<Range<u64>> {
    if entry.flags & FLAG_ENCRYPTED != 0 {
        return Err(unsupported(
            FMT,
            format!("{}: encrypted entries", entry.name),
        ));
    }
    let lh = read_vec(r, entry.local_header_offset, LOCAL_HEADER_LEN)
        .map_err(|_| invalid(FMT, format!("{}: local header out of range", entry.name)))?;
    let range = local_data_range(&lh, entry)?;
    if range.end > r.len() {
        return Err(invalid(
            FMT,
            format!("{}: data runs past end of file", entry.name),
        ));
    }
    Ok(range)
}

/// Read and decompress a whole entry. Supports stored (0) and zstd (93).
/// Deflate and other methods are handled by the `zip` crate elsewhere.
pub fn read_entry<R: RangeRead + ?Sized>(r: &R, entry: &ZipEntry) -> Result<Vec<u8>> {
    let range = entry_data_range(r, entry)?;
    let data = read_vec(r, range.start, range.end - range.start)?;
    let out = match entry.method {
        METHOD_STORED => data,
        METHOD_ZSTD => {
            use std::io::Read;
            let mut out = Vec::new();
            zstd::stream::read::Decoder::new(&data[..])?
                .take(entry.uncompressed_size.saturating_add(1))
                .read_to_end(&mut out)
                .map_err(|e| invalid(FMT, format!("{}: zstd: {e}", entry.name)))?;
            out
        }
        m => {
            return Err(unsupported(
                FMT,
                format!("{}: compression method {m}", entry.name),
            ));
        }
    };
    if out.len() as u64 != entry.uncompressed_size {
        return Err(invalid(
            FMT,
            format!(
                "{}: decompressed {} bytes, directory says {}",
                entry.name,
                out.len(),
                entry.uncompressed_size
            ),
        ));
    }
    Ok(out)
}

/// A method-93 entry whose data is a seekable zstd stream.
#[derive(Debug, Clone)]
pub struct SeekableEntry {
    pub entry: ZipEntry,
    /// Absolute range of the entry's compressed data in the zip.
    pub data: Range<u64>,
    pub table: SeekTable,
}

impl SeekableEntry {
    pub fn open<R: RangeRead + ?Sized>(r: &R, entry: &ZipEntry) -> Result<SeekableEntry> {
        if entry.method != METHOD_ZSTD {
            return Err(unsupported(
                FMT,
                format!("{}: method {} is not zstd (93)", entry.name, entry.method),
            ));
        }
        let data = entry_data_range(r, entry)?;
        let window = SubRange::new(r, data.clone())?;
        let table = SeekTable::read_from(&window).map_err(|e| match e {
            FormatError::Invalid { msg, .. } => invalid(FMT, format!("{}: {msg}", entry.name)),
            other => other,
        })?;
        if table.decompressed_len() != entry.uncompressed_size {
            return Err(invalid(
                FMT,
                format!(
                    "{}: seek table covers {} bytes, directory says {}",
                    entry.name,
                    table.decompressed_len(),
                    entry.uncompressed_size
                ),
            ));
        }
        Ok(SeekableEntry {
            entry: entry.clone(),
            data,
            table,
        })
    }

    /// Build from parts an async caller fetched itself: the entry, the
    /// absolute range of its data ([`local_data_range`]) and its frame map
    /// ([`ZipEntry::frame_map`] or [`SeekTable::parse`] of the data's tail).
    pub fn from_parts(
        entry: ZipEntry,
        data: Range<u64>,
        table: SeekTable,
    ) -> Result<SeekableEntry> {
        if entry.method != METHOD_ZSTD {
            return Err(unsupported(
                FMT,
                format!("{}: method {} is not zstd (93)", entry.name, entry.method),
            ));
        }
        if data.end.checked_sub(data.start) != Some(entry.compressed_size)
            || table.compressed_len() + table.table_len() != entry.compressed_size
            || table.decompressed_len() != entry.uncompressed_size
        {
            return Err(invalid(
                FMT,
                format!(
                    "{}: frame map ({} -> {} bytes) or data range {data:?} disagrees with the directory ({} -> {})",
                    entry.name,
                    table.compressed_len() + table.table_len(),
                    table.decompressed_len(),
                    entry.compressed_size,
                    entry.uncompressed_size
                ),
            ));
        }
        Ok(SeekableEntry { entry, data, table })
    }

    /// Frames covering `range` of the uncompressed entry.
    pub fn frames_for_range(&self, range: Range<u64>) -> Result<Vec<FrameEntry>> {
        self.table.frames_for_range(range)
    }

    /// Absolute range in the zip of a frame's compressed bytes.
    pub fn absolute(&self, frame: &FrameEntry) -> Range<u64> {
        let r = frame.compressed_range();
        self.data.start + r.start..self.data.start + r.end
    }

    /// Decompress `range` of the uncompressed entry.
    pub fn read<R: RangeRead + ?Sized>(&self, r: &R, range: Range<u64>) -> Result<Vec<u8>> {
        let window = SubRange::new(r, self.data.clone())?;
        seekable::read_range(&window, &self.table, range)
    }
}
