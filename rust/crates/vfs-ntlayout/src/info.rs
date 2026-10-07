//! The byte layouts of the file-information classes the shim answers itself, written once.
//!
//! `FileBasic`, `FileStandard`, `FileNetworkOpen` and `FileStat` are answered by path
//! (`NtQueryAttributesFile`, `NtQueryFullAttributesFile`, `NtQueryInformationByName`) and by
//! synthetic handle (`NtQueryInformationFile`). Every writer takes the start of the structure as a
//! slice and writes the fields little-endian, so the caller's buffer needs no alignment. A writer
//! panics if the slice is shorter than its structure: the hook checks the caller's length first
//! and declines or reports `STATUS_INFO_LENGTH_MISMATCH` itself. A writer writes every field of
//! its structure except padding after the last field; a caller that wants that zeroed zeroes the
//! slice first.

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// `FILE_INFORMATION_CLASS` values answered by path.
const CLASS_BASIC: u32 = 4;
const CLASS_STANDARD: u32 = 5;
const CLASS_NETWORK_OPEN: u32 = 34;
const CLASS_STAT: u32 = 68;
const CLASS_STAT_BASIC: u32 = 77;

/// `FILE_BASIC_INFORMATION` size.
pub const BASIC_LEN: usize = 40;
/// `FILE_STANDARD_INFORMATION` size.
pub const STANDARD_LEN: usize = 24;
/// `FILE_NETWORK_OPEN_INFORMATION` size.
pub const NETWORK_OPEN_LEN: usize = 56;
/// `FILE_STAT_INFORMATION` size.
pub const STAT_LEN: usize = 72;
/// `FILE_STAT_BASIC_INFORMATION` size.
pub const STAT_BASIC_LEN: usize = 104;
/// The fixed prefix of `FILE_ALL_INFORMATION` the shim fills: Basic 40 | Standard 24 | Internal 8
/// | Ea 4 | Access 4 | Position 8 | Mode 4 | Alignment 4 | Name 4.
pub const ALL_PREFIX_LEN: usize = 100;
/// `FILE_ID_INFORMATION` size.
pub const ID_LEN: usize = 24;
/// `FILE_ATTRIBUTE_TAG_INFORMATION` size.
pub const ATTRIBUTE_TAG_LEN: usize = 8;

/// `FileAttributes` of a synthetic file or directory.
pub fn attributes(is_dir: bool) -> u32 {
    if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    }
}

fn put_i64(b: &mut [u8], off: usize, v: i64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// `FILE_BASIC_INFORMATION` (40 bytes): four times, then `FileAttributes`. The reserved word
/// after it is not written.
pub fn put_basic(b: &mut [u8], time: i64, attrs: u32) {
    for off in [0, 8, 16, 24] {
        put_i64(b, off, time);
    }
    put_u32(b, 32, attrs);
}

/// `FILE_STANDARD_INFORMATION` (24 bytes): sizes, one link, nothing pending.
pub fn put_standard(b: &mut [u8], size: u64, is_dir: bool) {
    put_i64(b, 0, size as i64); // AllocationSize
    put_i64(b, 8, size as i64); // EndOfFile
    put_u32(b, 16, 1); // NumberOfLinks
    b[20] = 0; // DeletePending
    b[21] = u8::from(is_dir); // Directory
    b[22..24].fill(0); // padding
}

/// `FILE_NETWORK_OPEN_INFORMATION` (56 bytes): four times, sizes, `FileAttributes`. The reserved
/// word after it is not written.
pub fn put_network_open(b: &mut [u8], time: i64, size: u64, attrs: u32) {
    for off in [0, 8, 16, 24] {
        put_i64(b, off, time);
    }
    put_i64(b, 32, size as i64); // AllocationSize
    put_i64(b, 40, size as i64); // EndOfFile
    put_u32(b, 48, attrs);
}

/// `FILE_STAT_INFORMATION` (72 bytes), whose first 72 bytes `FILE_STAT_BASIC_INFORMATION` (104)
/// repeats: file id, four times, sizes, `FileAttributes`, `ReparseTag` (not written), one link,
/// `EffectiveAccess`.
pub fn put_stat(
    b: &mut [u8],
    file_id: i64,
    time: i64,
    size: u64,
    attrs: u32,
    effective_access: u32,
) {
    put_i64(b, 0, file_id);
    for off in [8, 16, 24, 32] {
        put_i64(b, off, time);
    }
    put_i64(b, 40, size as i64); // AllocationSize
    put_i64(b, 48, size as i64); // EndOfFile
    put_u32(b, 56, attrs);
    put_u32(b, 64, 1); // NumberOfLinks
    put_u32(b, 68, effective_access);
}

/// The answer to a stat-by-path call (`NtQueryInformationByName`) for `class_raw` (a
/// `FILE_INFORMATION_CLASS`): the structure zeroed, then written, with its length. `None`, with
/// nothing written, for a class the shim does not answer by path and for a buffer shorter than the
/// structure.
///
/// Windows 11 routes existence checks through class 77 (`FileStatBasicInformation`) instead of
/// `NtQueryFullAttributesFile`, so an unhooked build answers them from the real directory behind
/// the mount. That is silent by construction: the caller never opens anything, so nothing appears
/// in any open-side counter, and a game that tolerates a missing file simply skips it. Skyrim's
/// intro video and its master plugins both vanished this way.
///
/// Only the classes that are pure metadata are filled. Anything else under the root falls
/// through, which is no worse than before the hook existed.
pub fn by_name_info(class_raw: u32, buf: &mut [u8], is_dir: bool, size: u64) -> Option<usize> {
    let need = match class_raw {
        CLASS_BASIC => BASIC_LEN,
        CLASS_STANDARD => STANDARD_LEN,
        CLASS_NETWORK_OPEN => NETWORK_OPEN_LEN,
        CLASS_STAT => STAT_LEN,
        CLASS_STAT_BASIC => STAT_BASIC_LEN,
        _ => return None,
    };
    let buf = buf.get_mut(..need)?;
    buf.fill(0);
    let attrs = attributes(is_dir);
    match class_raw {
        CLASS_BASIC => put_basic(buf, 0, attrs),
        CLASS_STANDARD => put_standard(buf, size, is_dir),
        CLASS_NETWORK_OPEN => put_network_open(buf, 0, size, attrs),
        _ => put_stat(buf, 0, 0, size, attrs, 0),
    }
    Some(need)
}

/// The fixed prefix of `FILE_ALL_INFORMATION` (100 bytes), the trailing name left empty:
/// attributes (including DIRECTORY), size and the Standard `Directory` flag, `IndexNumber` at 64
/// and `CurrentByteOffset` at 80. The rest is zeroed. Times stay zero.
pub fn put_all_prefix(b: &mut [u8], is_dir: bool, size: u64, index_number: i64, position: i64) {
    let b = &mut b[..ALL_PREFIX_LEN];
    b.fill(0);
    put_basic(&mut b[..BASIC_LEN], 0, attributes(is_dir));
    put_standard(&mut b[BASIC_LEN..BASIC_LEN + STANDARD_LEN], size, is_dir);
    put_i64(b, 64, index_number);
    put_i64(b, 80, position);
}

/// `FILE_ID_INFORMATION` (24 bytes): `VolumeSerialNumber` at 0, the 128-bit `FileId` at 8 (the
/// low half is `file_id`, the high half zero).
pub fn put_id(b: &mut [u8], volume_serial: u64, file_id: i64) {
    let b = &mut b[..ID_LEN];
    b.fill(0);
    b[..8].copy_from_slice(&volume_serial.to_le_bytes());
    put_i64(b, 8, file_id);
}

/// `FILE_ATTRIBUTE_TAG_INFORMATION` (8 bytes): `FileAttributes`, then a `ReparseTag` of 0. Never a
/// reparse point.
pub fn put_attribute_tag(b: &mut [u8], is_dir: bool) {
    put_u32(b, 0, attributes(is_dir));
    put_u32(b, 4, 0);
}

/// How a caller's buffer compared with what a variable-length answer needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    /// Everything was written.
    Fits,
    /// The fixed part is there but the name is cut: `STATUS_BUFFER_OVERFLOW`.
    Overflow,
    /// Not even the fixed part fits: `STATUS_INFO_LENGTH_MISMATCH`. Nothing was written.
    Mismatch,
}

/// `FILE_NAME_INFORMATION` (a `u32` byte length, then the name, no NUL) for a buffer sized by the
/// caller. NT refuses a buffer smaller than the structure (8 bytes with its one-character name
/// field) outright; given one too small for the whole name it writes the full length, as much of
/// the name as fits, and says overflow. Returns the fit and the bytes written (the value for
/// `IoStatusBlock.Information`).
pub fn put_file_name(name: &str, buf: &mut [u8]) -> (Fit, usize) {
    if buf.len() < 8 {
        return (Fit::Mismatch, 0);
    }
    let units: Vec<u16> = name.encode_utf16().collect();
    let fits = units.len().min((buf.len() - 4) / 2);
    put_u32(buf, 0, (units.len() * 2) as u32);
    for (i, u) in units[..fits].iter().enumerate() {
        buf[4 + i * 2..6 + i * 2].copy_from_slice(&u.to_le_bytes());
    }
    let fit = if fits == units.len() {
        Fit::Fits
    } else {
        Fit::Overflow
    };
    (fit, 4 + fits * 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }
    fn i64_at(b: &[u8], o: usize) -> i64 {
        i64::from_le_bytes(b[o..o + 8].try_into().unwrap())
    }

    #[test]
    fn every_supported_class_reports_its_documented_length() {
        for (class, want) in [
            (CLASS_BASIC, 40usize),
            (CLASS_STANDARD, 24),
            (CLASS_NETWORK_OPEN, 56),
            (CLASS_STAT, 72),
            (CLASS_STAT_BASIC, 104),
        ] {
            let mut buf = vec![0u8; want];
            assert_eq!(
                by_name_info(class, &mut buf, false, 1),
                Some(want),
                "class {class}"
            );
        }
    }

    /// A short buffer must be declined, not partially written: the caller sized it for a
    /// different class and every byte past its end belongs to someone.
    #[test]
    fn a_buffer_one_byte_short_is_refused() {
        for (class, need) in [
            (CLASS_BASIC, 40usize),
            (CLASS_STANDARD, 24),
            (CLASS_NETWORK_OPEN, 56),
            (CLASS_STAT, 72),
            (CLASS_STAT_BASIC, 104),
        ] {
            let mut buf = vec![0xAAu8; need - 1];
            assert_eq!(
                by_name_info(class, &mut buf, false, 1),
                None,
                "class {class}"
            );
            assert!(
                buf.iter().all(|b| *b == 0xAA),
                "class {class} wrote into a short buffer"
            );
        }
    }

    #[test]
    fn an_unknown_class_is_declined_so_the_caller_falls_through() {
        let mut buf = vec![0u8; 512];
        assert_eq!(by_name_info(9999, &mut buf, false, 1), None);
    }

    /// The size a stat reports is the whole reason these classes are answered: a caller that
    /// sees zero bytes may skip the file without ever opening it. These offsets are ABI, not our
    /// choice, and a wrong one is silent.
    #[test]
    fn size_lands_at_the_offset_each_class_defines() {
        const SIZE: u64 = 249_753_412; // Skyrim.esm, i.e. well past 32 bits.
        let mut buf = vec![0u8; 104];

        by_name_info(CLASS_STANDARD, &mut buf, false, SIZE).unwrap();
        assert_eq!(i64_at(&buf, 0), SIZE as i64, "standard AllocationSize");
        assert_eq!(i64_at(&buf, 8), SIZE as i64, "standard EndOfFile");

        buf.fill(0);
        by_name_info(CLASS_NETWORK_OPEN, &mut buf, false, SIZE).unwrap();
        assert_eq!(i64_at(&buf, 32), SIZE as i64, "network-open AllocationSize");
        assert_eq!(i64_at(&buf, 40), SIZE as i64, "network-open EndOfFile");

        for class in [CLASS_STAT, CLASS_STAT_BASIC] {
            buf.fill(0);
            by_name_info(class, &mut buf, false, SIZE).unwrap();
            assert_eq!(
                i64_at(&buf, 40),
                SIZE as i64,
                "class {class} AllocationSize"
            );
            assert_eq!(i64_at(&buf, 48), SIZE as i64, "class {class} EndOfFile");
        }
    }

    #[test]
    fn directories_are_distinguishable_from_files_in_every_class() {
        let mut buf = vec![0u8; 104];
        for (class, attr_off) in [
            (CLASS_BASIC, 32usize),
            (CLASS_NETWORK_OPEN, 48),
            (CLASS_STAT, 56),
            (CLASS_STAT_BASIC, 56),
        ] {
            buf.fill(0);
            by_name_info(class, &mut buf, true, 0).unwrap();
            assert_eq!(
                u32_at(&buf, attr_off),
                FILE_ATTRIBUTE_DIRECTORY,
                "class {class}"
            );
            buf.fill(0);
            by_name_info(class, &mut buf, false, 1).unwrap();
            assert_eq!(
                u32_at(&buf, attr_off),
                FILE_ATTRIBUTE_NORMAL,
                "class {class}"
            );
        }
        // FileStandardInformation carries a boolean rather than an attribute.
        buf.fill(0);
        by_name_info(CLASS_STANDARD, &mut buf, true, 0).unwrap();
        assert_eq!(buf[21], 1, "standard Directory flag");
        buf.fill(0);
        by_name_info(CLASS_STANDARD, &mut buf, false, 1).unwrap();
        assert_eq!(buf[21], 0, "standard Directory flag set for a file");
    }

    #[test]
    fn basic_and_network_open_repeat_one_time_four_times() {
        let mut b = [0xEEu8; 40];
        put_basic(&mut b, 7, 0x10);
        assert_eq!(
            [i64_at(&b, 0), i64_at(&b, 8), i64_at(&b, 16), i64_at(&b, 24)],
            [7; 4]
        );
        assert_eq!(u32_at(&b, 32), 0x10);
        assert_eq!(&b[36..], &[0xEE; 4], "the reserved word is not written");

        let mut n = [0xEEu8; 56];
        put_network_open(&mut n, 5, 77, 0x80);
        assert_eq!((i64_at(&n, 0), i64_at(&n, 24)), (5, 5));
        assert_eq!((i64_at(&n, 32), i64_at(&n, 40)), (77, 77));
        assert_eq!(u32_at(&n, 48), 0x80);
        assert_eq!(&n[52..], &[0xEE; 4], "the reserved word is not written");
    }

    #[test]
    fn standard_has_one_link_and_zeroes_its_padding() {
        let mut s = [9u8; 24];
        put_standard(&mut s, 123, false);
        assert_eq!(
            (i64_at(&s, 0), i64_at(&s, 8), u32_at(&s, 16)),
            (123, 123, 1)
        );
        assert_eq!(&s[20..], &[0, 0, 0, 0]);
    }

    #[test]
    fn a_stat_has_the_documented_offsets() {
        let mut buf = [0u8; 72];
        put_stat(&mut buf, 11, 22, 33, 44, 55);
        assert_eq!(i64_at(&buf, 0), 11);
        assert_eq!(
            [
                i64_at(&buf, 8),
                i64_at(&buf, 16),
                i64_at(&buf, 24),
                i64_at(&buf, 32)
            ],
            [22; 4]
        );
        assert_eq!((i64_at(&buf, 40), i64_at(&buf, 48)), (33, 33));
        assert_eq!(
            (
                u32_at(&buf, 56),
                u32_at(&buf, 60),
                u32_at(&buf, 64),
                u32_at(&buf, 68)
            ),
            (44, 0, 1, 55)
        );
    }

    #[test]
    fn the_all_prefix_places_basic_standard_index_and_position() {
        let mut b = [0xFFu8; 100];
        put_all_prefix(&mut b, true, 4096, 99, 12);
        assert_eq!(u32_at(&b, 32), FILE_ATTRIBUTE_DIRECTORY); // Basic.FileAttributes
        assert_eq!(i64_at(&b, 0), 0); // Basic times stay zero
        assert_eq!(i64_at(&b, 40), 4096); // Standard.AllocationSize
        assert_eq!(b[40 + 21], 1); // Standard.Directory
        assert_eq!(i64_at(&b, 64), 99); // Internal.IndexNumber
        assert_eq!(i64_at(&b, 80), 12); // Position.CurrentByteOffset
        assert_eq!(
            &b[88..],
            &[0u8; 12],
            "Mode, Alignment and the name are empty"
        );
    }

    #[test]
    fn id_and_attribute_tag_layouts() {
        let mut b = [0xFFu8; 24];
        put_id(&mut b, 0x5646_5300, 42);
        assert_eq!(i64_at(&b, 0), 0x5646_5300);
        assert_eq!(i64_at(&b, 8), 42);
        assert_eq!(i64_at(&b, 16), 0, "the high half of the 128-bit id is zero");

        let mut t = [0xFFu8; 8];
        put_attribute_tag(&mut t, false);
        assert_eq!((u32_at(&t, 0), u32_at(&t, 4)), (FILE_ATTRIBUTE_NORMAL, 0));
    }

    #[test]
    fn a_name_that_fits_is_written_whole() {
        let mut b = [0u8; 64];
        let (fit, n) = put_file_name(r"\a\b", &mut b);
        assert_eq!((fit, n), (Fit::Fits, 4 + 8));
        assert_eq!(u32_at(&b, 0), 8);
        assert_eq!(&b[4..12], &[b'\\', 0, b'a', 0, b'\\', 0, b'b', 0]);
    }

    #[test]
    fn a_name_too_long_is_cut_with_its_full_length_reported() {
        let mut b = [0u8; 10]; // 6 bytes of name room: 3 of the 6 units
        let (fit, n) = put_file_name("abcdef", &mut b);
        assert_eq!((fit, n), (Fit::Overflow, 10));
        assert_eq!(u32_at(&b, 0), 12);
        assert_eq!(&b[4..10], &[b'a', 0, b'b', 0, b'c', 0]);
    }

    #[test]
    fn a_buffer_smaller_than_the_structure_is_a_length_mismatch() {
        let mut b = [0xAAu8; 7];
        assert_eq!(put_file_name("a", &mut b), (Fit::Mismatch, 0));
        assert_eq!(b, [0xAA; 7]);
        let mut b8 = [0u8; 8];
        assert_eq!(
            put_file_name("abc", &mut b8),
            (Fit::Overflow, 8),
            "two of three units"
        );
        assert_eq!(put_file_name("ab", &mut b8), (Fit::Fits, 8));
    }
}
