//! Directory-info and `FILE_NAME_INFORMATION` marshalling: pure byte layout, written into a
//! caller-supplied slice.

/// One entry in a directory listing — used both for the caller's real on-disk
/// entries and for the merged result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirItem {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

/// The timestamp reported for every VFS-backed file.
///
/// Not zero, and that is the whole point. A `FILETIME` of 0 is 1 January
/// 1601, and Cyberpunk 2077 refuses to start against it: it stats
/// `r6/cache/final.redscripts`, gets 1601, and puts up "encountered an error
/// caused by a corrupted or missing scripts file". Reporting a plausible date
/// instead takes it from that dialog to a running game window. Skyrim SE never
/// looked at a stat's time, which is why this survived until a second game
/// existed.
///
/// The value is arbitrary but must be **stable across runs** and **not in the
/// future**. Stability matters more than accuracy: a timestamp that moved every
/// launch would invalidate exactly the caches this exists to satisfy, and a
/// game that recompiles its script cache on every boot is no better off than
/// one that refuses to boot.
///
/// Not every provider has times: a Steam depot manifest has none, so ocm's
/// depot provider supplies `mtime: 0` honestly. A directory listing reports a
/// provider's own time where it has one and this where it has none
/// ([`write_dir_info`]); a stat, whose reply carries no time, always reports
/// this.
///
/// 2024-01-01T00:00:00Z, in 100 ns ticks since 1601.
pub const SYNTH_FILETIME: i64 = 133_485_408_000_000_000;

/// A provider's `mtime` (seconds since the Unix epoch) as a `FILETIME` (100 ns
/// ticks since 1601), or [`SYNTH_FILETIME`] when it has none (0 or less).
pub fn filetime_of(mtime: i64) -> i64 {
    /// Seconds from 1601-01-01 to 1970-01-01.
    const UNIX_EPOCH_IN_FILETIME_SECS: i64 = 11_644_473_600;
    if mtime <= 0 {
        return SYNTH_FILETIME;
    }
    mtime
        .checked_add(UNIX_EPOCH_IN_FILETIME_SECS)
        .and_then(|s| s.checked_mul(10_000_000))
        .unwrap_or(SYNTH_FILETIME)
}

/// The directory-info `FILE_INFORMATION_CLASS` values the shim marshals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirInfoClass {
    Directory,       // 1  FILE_DIRECTORY_INFORMATION
    FullDirectory,   // 2  FILE_FULL_DIR_INFORMATION
    BothDirectory,   // 3  FILE_BOTH_DIR_INFORMATION
    Names,           // 12 FILE_NAMES_INFORMATION
    IdBothDirectory, // 37 FILE_ID_BOTH_DIR_INFORMATION
    IdFullDirectory, // 38 FILE_ID_FULL_DIR_INFORMATION
}

impl DirInfoClass {
    /// Map a raw `FILE_INFORMATION_CLASS`; `None` for classes we do not marshal.
    pub fn from_u32(v: u32) -> Option<DirInfoClass> {
        Some(match v {
            1 => DirInfoClass::Directory,
            2 => DirInfoClass::FullDirectory,
            3 => DirInfoClass::BothDirectory,
            12 => DirInfoClass::Names,
            37 => DirInfoClass::IdBothDirectory,
            38 => DirInfoClass::IdFullDirectory,
            _ => return None,
        })
    }

    /// Byte offset of the `FileName` field == the fixed header size.
    fn name_offset(self) -> usize {
        match self {
            DirInfoClass::Names => 12,
            DirInfoClass::Directory => 64,
            DirInfoClass::FullDirectory => 68,
            DirInfoClass::IdFullDirectory => 80,
            DirInfoClass::BothDirectory => 94,
            DirInfoClass::IdBothDirectory => 104,
        }
    }

    /// Byte offset of the `FileNameLength` (u32) field.
    fn name_len_offset(self) -> usize {
        match self {
            DirInfoClass::Names => 8,
            _ => 60,
        }
    }

    /// Whether this class carries `EndOfFile`/`AllocationSize`/`FileAttributes`.
    fn has_metadata(self) -> bool {
        !matches!(self, DirInfoClass::Names)
    }
}

/// The NTSTATUS family a directory write resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirStatus {
    Success,
    NoMoreFiles,
    BufferOverflow,
}

/// Result of marshalling directory entries into a caller buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirWriteResult {
    /// Bytes actually used (end offset of the last record's data) —
    /// the value to report as `IoStatusBlock.Information`.
    pub bytes: usize,
    /// Number of entries written.
    pub count: usize,
    pub status: DirStatus,
}

/// Marshal `items` into `buf` in the layout of `class`, chaining
/// `NextEntryOffset`, 8-byte aligning each record, stopping at `single` (one
/// entry) or when the next record would overflow `buf`. Pure: writes only into
/// `buf`.
pub fn write_dir_info(
    class: DirInfoClass,
    items: &[DirItem],
    buf: &mut [u8],
    single: bool,
) -> DirWriteResult {
    let name_off = class.name_offset();
    let name_len_off = class.name_len_offset();
    let cap = buf.len();
    let mut off = 0usize;
    let mut count = 0usize;
    let mut prev: Option<usize> = None;
    let mut last_end = 0usize;

    for it in items {
        let name16: Vec<u16> = it.name.encode_utf16().collect();
        let namelen = name16.len() * 2;
        let rec = name_off + namelen;
        if off + rec > cap {
            break;
        }
        // Zero the fixed header (EaSize/ShortName/FileId fields left zero).
        for b in &mut buf[off..off + name_off] {
            *b = 0;
        }
        if class.has_metadata() {
            // CreationTime, LastAccessTime, LastWriteTime, ChangeTime.
            let time = filetime_of(it.mtime);
            for t in [8, 16, 24, 32] {
                buf[off + t..off + t + 8].copy_from_slice(&time.to_le_bytes());
            }
            let eof = it.size as i64;
            buf[off + 40..off + 48].copy_from_slice(&eof.to_le_bytes());
            buf[off + 48..off + 56].copy_from_slice(&eof.to_le_bytes());
            let attrs: u32 = if it.is_dir { 0x10 } else { 0x80 };
            buf[off + 56..off + 60].copy_from_slice(&attrs.to_le_bytes());
        }
        buf[off + name_len_off..off + name_len_off + 4]
            .copy_from_slice(&(namelen as u32).to_le_bytes());
        let name_bytes: Vec<u8> = name16.iter().flat_map(|u| u.to_le_bytes()).collect();
        buf[off + name_off..off + name_off + namelen].copy_from_slice(&name_bytes);

        if let Some(p) = prev {
            let delta = (off - p) as u32;
            buf[p..p + 4].copy_from_slice(&delta.to_le_bytes());
        }
        prev = Some(off);
        last_end = off + rec;
        count += 1;
        off += (rec + 7) & !7; // 8-byte align next record
        if single {
            break;
        }
    }

    let status = if count == 0 {
        if items.is_empty() {
            DirStatus::NoMoreFiles
        } else {
            DirStatus::BufferOverflow
        }
    } else {
        DirStatus::Success
    };
    DirWriteResult {
        bytes: last_end,
        count,
        status,
    }
}

/// Result of marshalling a `FILE_NAME_INFORMATION` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NameWriteResult {
    pub bytes: usize,
    pub status: DirStatus,
}

/// Marshal a `FILE_NAME_INFORMATION` / `FILE_NORMALIZED_NAME_INFORMATION`:
/// `FileNameLength` (u32 bytes) @0, UTF-16LE `FileName` (no NUL) @4. On overflow
/// writes only `FileNameLength` (documented behavior).
pub fn write_file_name_info(name: &str, buf: &mut [u8]) -> NameWriteResult {
    let name16: Vec<u16> = name.encode_utf16().collect();
    let namelen = name16.len() * 2;
    if buf.len() < 4 {
        return NameWriteResult {
            bytes: 0,
            status: DirStatus::BufferOverflow,
        };
    }
    buf[0..4].copy_from_slice(&(namelen as u32).to_le_bytes());
    if buf.len() < 4 + namelen {
        return NameWriteResult {
            bytes: 4,
            status: DirStatus::BufferOverflow,
        };
    }
    let nb: Vec<u8> = name16.iter().flat_map(|u| u.to_le_bytes()).collect();
    buf[4..4 + namelen].copy_from_slice(&nb);
    NameWriteResult {
        bytes: 4 + namelen,
        status: DirStatus::Success,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ditem(name: &str, is_dir: bool, size: u64) -> DirItem {
        DirItem {
            name: name.into(),
            is_dir,
            size,
            mtime: 0,
        }
    }

    fn ru32(buf: &[u8], rec: usize, off: usize) -> u32 {
        u32::from_le_bytes(buf[rec + off..rec + off + 4].try_into().unwrap())
    }
    fn ri64(buf: &[u8], rec: usize, off: usize) -> i64 {
        i64::from_le_bytes(buf[rec + off..rec + off + 8].try_into().unwrap())
    }
    fn rname(buf: &[u8], rec: usize, name_off: usize, namelen: usize) -> String {
        let units: Vec<u16> = buf[rec + name_off..rec + name_off + namelen]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    }

    #[test]
    fn write_full_dir_two_entries_chained() {
        let items = [ditem("a.esp", false, 5), ditem("sub", true, 0)];
        let mut buf = vec![0u8; 1024];
        let r = write_dir_info(DirInfoClass::FullDirectory, &items, &mut buf, false);
        assert_eq!(r.status, DirStatus::Success);
        assert_eq!(r.count, 2);
        assert_eq!(ru32(&buf, 0, 60), 10); // "a.esp" = 5 chars * 2 bytes
        assert_eq!(ri64(&buf, 0, 40), 5); // EndOfFile
        assert_eq!(ru32(&buf, 0, 56), 0x80); // FILE_ATTRIBUTE_NORMAL
        assert_eq!(rname(&buf, 0, 68, 10), "a.esp");
        let next = ru32(&buf, 0, 0) as usize;
        assert_eq!(next, 80); // (68+10)=78 -> 8-align -> 80
        assert_eq!(ru32(&buf, next, 56), 0x10); // second is a directory
        assert_eq!(rname(&buf, next, 68, 6), "sub");
        assert_eq!(ru32(&buf, next, 0), 0); // last record: NextEntryOffset 0
        assert_eq!(r.bytes, 80 + 68 + 6);
    }

    /// Every timestamp of a listed entry is its provider's mtime, as a
    /// `FILETIME`. They used to be left zero, 1 January 1601 for every file
    /// `FindFirstFile` found: Skyrim, listing its saves so, offered no
    /// Continue or Load over the saves the last session wrote and numbered
    /// the next one `Save1` again. Listed with their real times, it offers
    /// both.
    #[test]
    fn listed_entries_carry_the_providers_mtime() {
        // 2026-10-03T21:13:45Z.
        let mtime = 1_791_062_025;
        let want = (mtime + 11_644_473_600) * 10_000_000;
        let save = DirItem {
            name: "Save1.ess".into(),
            is_dir: false,
            size: 9,
            mtime,
        };
        for class in [
            DirInfoClass::Directory,
            DirInfoClass::FullDirectory,
            DirInfoClass::BothDirectory,
            DirInfoClass::IdBothDirectory,
            DirInfoClass::IdFullDirectory,
        ] {
            let mut buf = vec![0u8; 512];
            let r = write_dir_info(class, std::slice::from_ref(&save), &mut buf, false);
            assert_eq!(r.count, 1);
            // CreationTime, LastAccessTime, LastWriteTime, ChangeTime.
            for off in [8, 16, 24, 32] {
                assert_eq!(ri64(&buf, 0, off), want, "{class:?} at {off}");
            }
            assert_eq!(ri64(&buf, 0, 40), 9, "{class:?}: EndOfFile");
        }
    }

    /// A provider with no time (`mtime` 0: a depot manifest has none) is
    /// listed at [`SYNTH_FILETIME`], the time a stat of it reports, never
    /// at 1601.
    #[test]
    fn an_entry_without_a_time_is_listed_at_the_synthetic_time() {
        let mut buf = vec![0u8; 512];
        let r = write_dir_info(
            DirInfoClass::BothDirectory,
            &[ditem("a.esp", false, 1), ditem("sub", true, 0)],
            &mut buf,
            false,
        );
        assert_eq!(r.count, 2);
        let next = ru32(&buf, 0, 0) as usize;
        for rec in [0, next] {
            for off in [8, 16, 24, 32] {
                assert_eq!(ri64(&buf, rec, off), SYNTH_FILETIME);
            }
        }
    }

    #[test]
    fn write_both_dir_uses_class3_header() {
        let items = [ditem("x", false, 1)];
        let mut buf = vec![0u8; 512];
        let r = write_dir_info(DirInfoClass::BothDirectory, &items, &mut buf, false);
        assert_eq!(r.status, DirStatus::Success);
        assert_eq!(ru32(&buf, 0, 60), 2);
        assert_eq!(ru32(&buf, 0, 56), 0x80);
        assert_eq!(rname(&buf, 0, 94, 2), "x");
    }

    #[test]
    fn write_names_class_is_name_only() {
        let items = [ditem("only.txt", false, 999)];
        let mut buf = vec![0u8; 256];
        let r = write_dir_info(DirInfoClass::Names, &items, &mut buf, false);
        assert_eq!(r.status, DirStatus::Success);
        assert_eq!(ru32(&buf, 0, 8), 16); // "only.txt" = 8*2
        assert_eq!(rname(&buf, 0, 12, 16), "only.txt");
    }

    #[test]
    fn write_single_entry_stops_after_one() {
        let items = [ditem("a", false, 1), ditem("b", false, 1)];
        let mut buf = vec![0u8; 512];
        let r = write_dir_info(DirInfoClass::FullDirectory, &items, &mut buf, true);
        assert_eq!(r.count, 1);
        assert_eq!(r.status, DirStatus::Success);
        assert_eq!(ru32(&buf, 0, 0), 0); // single -> no chain
    }

    #[test]
    fn write_empty_is_no_more_files() {
        let mut buf = vec![0u8; 128];
        let r = write_dir_info(DirInfoClass::FullDirectory, &[], &mut buf, false);
        assert_eq!(r.count, 0);
        assert_eq!(r.status, DirStatus::NoMoreFiles);
        assert_eq!(r.bytes, 0);
    }

    #[test]
    fn write_too_small_for_first_is_buffer_overflow() {
        let items = [ditem("longname.esp", false, 1)];
        let mut buf = vec![0u8; 8]; // smaller than one class-2 record
        let r = write_dir_info(DirInfoClass::FullDirectory, &items, &mut buf, false);
        assert_eq!(r.count, 0);
        assert_eq!(r.status, DirStatus::BufferOverflow);
    }

    #[test]
    fn dir_info_class_from_u32() {
        assert_eq!(DirInfoClass::from_u32(2), Some(DirInfoClass::FullDirectory));
        assert_eq!(DirInfoClass::from_u32(3), Some(DirInfoClass::BothDirectory));
        assert_eq!(DirInfoClass::from_u32(12), Some(DirInfoClass::Names));
        assert_eq!(DirInfoClass::from_u32(99), None);
    }

    #[test]
    fn write_file_name_info_round_trips() {
        let mut buf = vec![0u8; 128];
        let r = write_file_name_info(r"\Games\Skyrim\Data\foo.esp", &mut buf);
        assert_eq!(r.status, DirStatus::Success);
        let namelen = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
        assert_eq!(
            namelen,
            r"\Games\Skyrim\Data\foo.esp".encode_utf16().count() * 2
        );
        let units: Vec<u16> = buf[4..4 + namelen]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        assert_eq!(
            String::from_utf16_lossy(&units),
            r"\Games\Skyrim\Data\foo.esp"
        );
        assert_eq!(r.bytes, 4 + namelen);
    }

    #[test]
    fn write_file_name_info_overflow_writes_length_only() {
        let mut buf = vec![0u8; 6]; // room for u32 len but not the name
        let r = write_file_name_info("abcdef", &mut buf);
        assert_eq!(r.status, DirStatus::BufferOverflow);
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 12);
    }
}
