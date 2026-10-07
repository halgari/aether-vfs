//! The byte layouts of the file-information classes the shim answers itself, written once.
//!
//! `FileBasic`, `FileStandard`, `FileNetworkOpen` and `FileStat` are answered by path
//! (`file_attr`: `NtQueryAttributesFile`, `NtQueryFullAttributesFile`,
//! `NtQueryInformationByName`) and by synthetic handle (`file_info`:
//! `NtQueryInformationFile`). The writers here take a pointer to the start of the structure and
//! write the fields, with unaligned stores because a caller's buffer has no alignment
//! guarantee. A writer writes every field of its structure except padding the structure has
//! after its last field; a caller that wants that zeroed zeroes the buffer first.
#![deny(unsafe_op_in_unsafe_fn)]

use crate::ntdef::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};

/// `FileAttributes` of a synthetic file or directory.
pub(super) fn attributes(is_dir: bool) -> u32 {
    if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    }
}

unsafe fn put_i64(p: *mut u8, off: usize, v: i64) {
    // SAFETY: the caller's buffer holds the structure (hook/mod.rs).
    unsafe { core::ptr::write_unaligned(p.add(off) as *mut i64, v) };
}

unsafe fn put_u32(p: *mut u8, off: usize, v: u32) {
    // SAFETY: the caller's buffer holds the structure (hook/mod.rs).
    unsafe { core::ptr::write_unaligned(p.add(off) as *mut u32, v) };
}

/// `FILE_BASIC_INFORMATION` (40 bytes): four times, then `FileAttributes`. The reserved word
/// after it is not written.
pub(super) unsafe fn put_basic(p: *mut u8, time: i64, attrs: u32) {
    for off in [0, 8, 16, 24] {
        // SAFETY: the caller's buffer holds the structure (hook/mod.rs).
        unsafe { put_i64(p, off, time) };
    }
    // SAFETY: as above.
    unsafe { put_u32(p, 32, attrs) };
}

/// `FILE_STANDARD_INFORMATION` (24 bytes): sizes, one link, nothing pending.
pub(super) unsafe fn put_standard(p: *mut u8, size: u64, is_dir: bool) {
    // SAFETY: the caller's buffer holds the structure (hook/mod.rs).
    unsafe {
        put_i64(p, 0, size as i64); // AllocationSize
        put_i64(p, 8, size as i64); // EndOfFile
        put_u32(p, 16, 1); // NumberOfLinks
        core::ptr::write_unaligned(p.add(20), 0u8); // DeletePending
        core::ptr::write_unaligned(p.add(21), u8::from(is_dir)); // Directory
        core::ptr::write_unaligned(p.add(22) as *mut u16, 0); // padding
    }
}

/// `FILE_NETWORK_OPEN_INFORMATION` (56 bytes): four times, sizes, `FileAttributes`. The reserved
/// word after it is not written.
pub(super) unsafe fn put_network_open(p: *mut u8, time: i64, size: u64, attrs: u32) {
    for off in [0, 8, 16, 24] {
        // SAFETY: the caller's buffer holds the structure (hook/mod.rs).
        unsafe { put_i64(p, off, time) };
    }
    // SAFETY: as above.
    unsafe {
        put_i64(p, 32, size as i64); // AllocationSize
        put_i64(p, 40, size as i64); // EndOfFile
        put_u32(p, 48, attrs);
    }
}

/// `FILE_STAT_INFORMATION` (72 bytes), whose first 72 bytes `FILE_STAT_BASIC_INFORMATION` (104)
/// repeats: file id, four times, sizes, `FileAttributes`, `ReparseTag` (not written), one link,
/// `EffectiveAccess`.
pub(super) unsafe fn put_stat(
    p: *mut u8,
    file_id: i64,
    time: i64,
    size: u64,
    attrs: u32,
    effective_access: u32,
) {
    // SAFETY: the caller's buffer holds the structure (hook/mod.rs).
    unsafe {
        put_i64(p, 0, file_id);
        for off in [8, 16, 24, 32] {
            put_i64(p, off, time);
        }
        put_i64(p, 40, size as i64); // AllocationSize
        put_i64(p, 48, size as i64); // EndOfFile
        put_u32(p, 56, attrs);
        put_u32(p, 64, 1); // NumberOfLinks
        put_u32(p, 68, effective_access);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntdef::{FileBasicInformation, FileNetworkOpenInformation, FileStandardInformation};
    use core::mem::{offset_of, size_of};

    /// The structures the by-handle and by-path answers used to write field by field.
    #[test]
    fn the_offsets_are_the_structures_fields() {
        assert_eq!(size_of::<FileBasicInformation>(), 40);
        assert_eq!(offset_of!(FileBasicInformation, creation_time), 0);
        assert_eq!(offset_of!(FileBasicInformation, last_access_time), 8);
        assert_eq!(offset_of!(FileBasicInformation, last_write_time), 16);
        assert_eq!(offset_of!(FileBasicInformation, change_time), 24);
        assert_eq!(offset_of!(FileBasicInformation, file_attributes), 32);

        assert_eq!(size_of::<FileStandardInformation>(), 24);
        assert_eq!(offset_of!(FileStandardInformation, allocation_size), 0);
        assert_eq!(offset_of!(FileStandardInformation, end_of_file), 8);
        assert_eq!(offset_of!(FileStandardInformation, number_of_links), 16);
        assert_eq!(offset_of!(FileStandardInformation, delete_pending), 20);
        assert_eq!(offset_of!(FileStandardInformation, directory), 21);
        assert_eq!(offset_of!(FileStandardInformation, _pad), 22);

        assert_eq!(size_of::<FileNetworkOpenInformation>(), 56);
        assert_eq!(offset_of!(FileNetworkOpenInformation, creation_time), 0);
        assert_eq!(offset_of!(FileNetworkOpenInformation, change_time), 24);
        assert_eq!(offset_of!(FileNetworkOpenInformation, allocation_size), 32);
        assert_eq!(offset_of!(FileNetworkOpenInformation, end_of_file), 40);
        assert_eq!(offset_of!(FileNetworkOpenInformation, file_attributes), 48);
    }

    #[test]
    fn the_writers_fill_the_typed_structures() {
        let mut b: FileBasicInformation = unsafe { core::mem::zeroed() };
        unsafe { put_basic(&mut b as *mut _ as *mut u8, 7, attributes(true)) };
        assert_eq!(
            (
                b.creation_time,
                b.last_access_time,
                b.last_write_time,
                b.change_time
            ),
            (7, 7, 7, 7)
        );
        assert_eq!(b.file_attributes, FILE_ATTRIBUTE_DIRECTORY);

        let mut s = FileStandardInformation {
            allocation_size: -1,
            end_of_file: -1,
            number_of_links: 9,
            delete_pending: 9,
            directory: 9,
            _pad: 9,
        };
        unsafe { put_standard(&mut s as *mut _ as *mut u8, 123, false) };
        assert_eq!(
            (s.allocation_size, s.end_of_file, s.number_of_links),
            (123, 123, 1)
        );
        assert_eq!((s.delete_pending, s.directory, s._pad), (0, 0, 0));

        let mut n: FileNetworkOpenInformation = unsafe { core::mem::zeroed() };
        unsafe { put_network_open(&mut n as *mut _ as *mut u8, 5, 77, attributes(false)) };
        assert_eq!((n.creation_time, n.change_time), (5, 5));
        assert_eq!((n.allocation_size, n.end_of_file), (77, 77));
        assert_eq!(n.file_attributes, FILE_ATTRIBUTE_NORMAL);
    }

    #[test]
    fn a_stat_has_the_documented_offsets() {
        let mut buf = [0u8; 72];
        unsafe { put_stat(buf.as_mut_ptr(), 11, 22, 33, 44, 55) };
        let i64_at = |o: usize| i64::from_le_bytes(buf[o..o + 8].try_into().unwrap());
        let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().unwrap());
        assert_eq!(i64_at(0), 11);
        assert_eq!([i64_at(8), i64_at(16), i64_at(24), i64_at(32)], [22; 4]);
        assert_eq!((i64_at(40), i64_at(48)), (33, 33));
        assert_eq!(
            (u32_at(56), u32_at(60), u32_at(64), u32_at(68)),
            (44, 0, 1, 55)
        );
    }
}
