//! Attribute queries by path: `NtQueryAttributesFile`, `NtQueryFullAttributesFile`, `NtQueryInformationByName`.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{
    ENGINE, TRAMP_QATTR, TRAMP_QFULL, TRAMP_QIBN, allow_disk_fallthrough, attributes,
    in_hook_reenter, path_file_id, path_of, put_basic, put_network_open, put_standard, put_stat,
};
use crate::ntdef::{
    FileBasicInformation, FileNetworkOpenInformation, ObjectAttributes,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};
use crate::overlay::OverlayState;
use core::ffi::c_void;
use vfs_redirect::SYNTH_FILETIME;
use windows_sys::Win32::Foundation::NTSTATUS;

/// Path-based getattr via director OP_GETATTR when FUSE client is live.
/// `Some(...)` means the path is under the managed root — caller must not tramp
/// to the Steam tree on NOT_FOUND (seal under-root).
unsafe fn fuse_path_attr(path: &str) -> Option<Result<(bool, u64, i64), i32>> {
    let client = crate::director::global()?;
    let (root, vp) = client.route(path)?;
    Some(match client.getattr(root, &vp) {
        Ok(a) if a.found => Ok((a.is_dir, a.size, a.mtime)),
        Ok(_) => Err(vfs_protocol::ST_NOT_FOUND),
        Err(st) => Err(st),
    })
}

/// Stat-by-path, with no handle anywhere in the call.
///
/// Windows 11 routes existence checks here (class 77,
/// `FileStatBasicInformation`) instead of `NtQueryFullAttributesFile`, so an
/// unhooked build answers them from the real directory behind the mount. That
/// is silent by construction: the caller never opens anything, so nothing
/// appears in any open-side counter, and a game that tolerates a missing file
/// simply skips it. Skyrim's intro video and its master plugins both vanished
/// this way.
///
/// Only the classes that are pure metadata are filled. Anything else under the
/// root falls through, which is no worse than before this hook existed.
unsafe fn fill_by_name(
    class_raw: u32,
    info: *mut c_void,
    length: u32,
    is_dir: bool,
    size: u64,
) -> Option<usize> {
    let attrs = attributes(is_dir);
    // Byte layouts per FILE_INFORMATION_CLASS. Written field-by-field with
    // unaligned writes because the caller's buffer has no alignment guarantee.
    let need: usize = match class_raw {
        4 => 40,   // FileBasicInformation
        5 => 24,   // FileStandardInformation
        34 => 56,  // FileNetworkOpenInformation
        68 => 72,  // FileStatInformation
        77 => 104, // FileStatBasicInformation (Win11)
        _ => return None,
    };
    if info.is_null() || (length as usize) < need {
        return None;
    }
    let p = info as *mut u8;
    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
    unsafe { core::ptr::write_bytes(p, 0, need) };
    // SAFETY: `info` holds `need` bytes (checked above); the writers' contract (hook/mod.rs).
    unsafe {
        match class_raw {
            4 => put_basic(p, 0, attrs),
            5 => put_standard(p, size, is_dir),
            34 => put_network_open(p, 0, size, attrs),
            68 | 77 => put_stat(p, 0, 0, size, attrs, 0),
            _ => return None,
        }
    }
    Some(need)
}

pub(super) unsafe fn qibn_hook_body(
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class_raw: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QByName);
    let tramp = match TRAMP_QIBN.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if in_hook_reenter() {
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(oa, iosb, info, length, class_raw) };
    }
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    if let Some(path) = match unsafe { path_of(oa) } {
        Ok(p) => p,
        Err(st) => return st,
    } {
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        let fuse = unsafe { fuse_path_attr(&path) };
        if fuse.is_none() {
            // Logged too: a stat that lands outside the root is exactly how a
            // wrong Data directory would present, and it is otherwise silent.
            crate::hookstats::note_stat(&path, &format!("byname{class_raw}-outside"));
        }
        if let Some(res) = fuse {
            match res {
                Ok((is_dir, size, _mtime)) => {
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    if let Some(n) = unsafe { fill_by_name(class_raw, info, length, is_dir, size) }
                    {
                        // Classes 68 and 77 open with the file id. By handle
                        // it is the path's id; by name it must be the same
                        // number, not zero.
                        if matches!(class_raw, 68 | 77) {
                            if let Some(id) = path_file_id(&path) {
                                // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
                                unsafe { core::ptr::write_unaligned(info as *mut i64, id) };
                            }
                        }
                        crate::hookstats::note_stat(&path, &format!("byname{class_raw}-ok"));
                        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                        unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, n) };
                        return STATUS_SUCCESS;
                    }
                    crate::hookstats::note_stat(&path, &format!("byname{class_raw}-UNSUP"));
                }
                Err(st) if st == vfs_protocol::ST_NOT_FOUND => {
                    crate::hookstats::note_stat(&path, &format!("byname{class_raw}-missing"));
                    if !allow_disk_fallthrough() {
                        return STATUS_OBJECT_NAME_NOT_FOUND;
                    }
                }
                Err(_) => return STATUS_UNSUCCESSFUL,
            }
        }
        // Task 4: the local snapshot no longer answers attribute queries (that
        // was `RootMap::query_attributes`/`AttrDecision`, both deleted) — the
        // director already had first refusal via `fuse_path_attr` above. The
        // shim-local write overlay (gate 4's mechanism) is the only thing
        // left that can still answer without the director, since it holds
        // content the director never sees (a just-created/modified file, or
        // a runtime delete's whiteout).
        if let Some(engine) = ENGINE.get() {
            match engine.overlay_state(&path) {
                Some(OverlayState::Present { is_dir, size, .. }) => {
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    if let Some(n) = unsafe { fill_by_name(class_raw, info, length, is_dir, size) }
                    {
                        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                        unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, n) };
                        return STATUS_SUCCESS;
                    }
                }
                Some(OverlayState::Whiteout) => return STATUS_OBJECT_NAME_NOT_FOUND,
                Some(OverlayState::Absent) | None => {}
            }
        }
    }
    // SAFETY: the original NT function, called with valid NT arguments.
    unsafe { tramp(oa, iosb, info, length, class_raw) }
}

pub(super) unsafe fn qattr_hook_body(
    oa: *const ObjectAttributes,
    info: *mut FileBasicInformation,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QAttr);
    let tramp = match TRAMP_QATTR.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if in_hook_reenter() {
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(oa, info) };
    }
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    if let Some(path) = match unsafe { path_of(oa) } {
        Ok(p) => p,
        Err(st) => return st,
    } {
        // Under-root: director only (zip/overrides). Never host Steam metadata.
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        let fuse = unsafe { fuse_path_attr(&path) };
        if fuse.is_none() {
            crate::hookstats::note_stat(&path, "outside-root");
        }
        if let Some(res) = fuse {
            crate::hookstats::note_stat(
                &path,
                match &res {
                    Ok(_) => "found",
                    Err(st) if *st == vfs_protocol::ST_NOT_FOUND => "NOT-FOUND",
                    Err(_) => "ERROR",
                },
            );
            match res {
                Ok((is_dir, _size, _mtime)) => {
                    if !info.is_null() {
                        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                        unsafe { put_basic(info as *mut u8, SYNTH_FILETIME, attributes(is_dir)) };
                    }
                    return STATUS_SUCCESS;
                }
                Err(st) if st == vfs_protocol::ST_NOT_FOUND => {
                    if allow_disk_fallthrough() {
                        // fall through to engine / tramp
                    } else {
                        return STATUS_OBJECT_NAME_NOT_FOUND;
                    }
                }
                Err(_) => return STATUS_UNSUCCESSFUL,
            }
        }
        // Task 4: overlay-only fallback (see `qibn_hook`'s comment on the
        // equivalent branch) — no more local snapshot answering here.
        if let Some(engine) = ENGINE.get() {
            match engine.overlay_state(&path) {
                Some(OverlayState::Present { is_dir, .. }) => {
                    if !info.is_null() {
                        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                        unsafe { put_basic(info as *mut u8, SYNTH_FILETIME, attributes(is_dir)) };
                    }
                    return STATUS_SUCCESS;
                }
                Some(OverlayState::Whiteout) => return STATUS_OBJECT_NAME_NOT_FOUND,
                Some(OverlayState::Absent) | None => {}
            }
        }
    }
    // SAFETY: the original NT function, called with valid NT arguments.
    unsafe { tramp(oa, info) }
}

pub(super) unsafe fn qfull_hook_body(
    oa: *const ObjectAttributes,
    info: *mut FileNetworkOpenInformation,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QFull);
    let tramp = match TRAMP_QFULL.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if in_hook_reenter() {
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(oa, info) };
    }
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    if let Some(path) = match unsafe { path_of(oa) } {
        Ok(p) => p,
        Err(st) => return st,
    } {
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        let fuse = unsafe { fuse_path_attr(&path) };
        if fuse.is_none() {
            crate::hookstats::note_stat(&path, "outside-root");
        }
        if let Some(res) = fuse {
            crate::hookstats::note_stat(
                &path,
                match &res {
                    Ok(_) => "found",
                    Err(st) if *st == vfs_protocol::ST_NOT_FOUND => "NOT-FOUND",
                    Err(_) => "ERROR",
                },
            );
            match res {
                Ok((is_dir, size, _mtime)) => {
                    if !info.is_null() {
                        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                        unsafe {
                            put_network_open(
                                info as *mut u8,
                                SYNTH_FILETIME,
                                size,
                                attributes(is_dir),
                            )
                        };
                    }
                    return STATUS_SUCCESS;
                }
                Err(st) if st == vfs_protocol::ST_NOT_FOUND => {
                    if !allow_disk_fallthrough() {
                        return STATUS_OBJECT_NAME_NOT_FOUND;
                    }
                }
                Err(_) => return STATUS_UNSUCCESSFUL,
            }
        }
        // Task 4: overlay-only fallback (see `qibn_hook`'s comment on the
        // equivalent branch) — no more local snapshot answering here.
        if let Some(engine) = ENGINE.get() {
            match engine.overlay_state(&path) {
                Some(OverlayState::Present { is_dir, size, .. }) => {
                    if !info.is_null() {
                        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                        unsafe {
                            put_network_open(
                                info as *mut u8,
                                SYNTH_FILETIME,
                                size,
                                attributes(is_dir),
                            )
                        };
                    }
                    return STATUS_SUCCESS;
                }
                Some(OverlayState::Whiteout) => return STATUS_OBJECT_NAME_NOT_FOUND,
                Some(OverlayState::Absent) | None => {}
            }
        }
    }
    // SAFETY: the original NT function, called with valid NT arguments.
    unsafe { tramp(oa, info) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ntdef::FILE_ATTRIBUTE_DIRECTORY;

    const CLASS_BASIC: u32 = 4;
    const CLASS_STANDARD: u32 = 5;
    const CLASS_NETWORK_OPEN: u32 = 34;
    const CLASS_STAT: u32 = 68;
    const CLASS_STAT_BASIC: u32 = 77;

    fn fill(class: u32, buf: &mut [u8], is_dir: bool, size: u64) -> Option<usize> {
        unsafe {
            fill_by_name(
                class,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                is_dir,
                size,
            )
        }
    }

    fn u32_at(buf: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
    }

    fn i64_at(buf: &[u8], off: usize) -> i64 {
        i64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
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
            assert_eq!(fill(class, &mut buf, false, 1), Some(want), "class {class}");
        }
    }

    /// A short buffer must be declined, not partially written: the caller sized
    /// it for a different class and every byte past its end belongs to someone.
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
            assert_eq!(fill(class, &mut buf, false, 1), None, "class {class}");
            assert!(
                buf.iter().all(|b| *b == 0xAA),
                "class {class} wrote into a short buffer"
            );
        }
    }

    #[test]
    fn an_unknown_class_is_declined_so_the_caller_falls_through() {
        let mut buf = vec![0u8; 512];
        assert_eq!(fill(9999, &mut buf, false, 1), None);
    }

    /// The size a stat reports is the whole reason these classes are answered:
    /// a caller that sees zero bytes may skip the file without ever opening it.
    /// Offsets and sizes of the metadata classes we answer by path.
    ///
    /// These are ABI, not our choice: the caller allocated the buffer and reads
    /// the fields at fixed offsets. Writing `EndOfFile` at the wrong offset does
    /// not fail — it reports a file of the wrong size, or a size of zero, which
    /// a caller is free to treat as "not worth opening". That is silent, so it
    /// gets pinned down here.
    #[test]
    fn size_lands_at_the_offset_each_class_defines() {
        const SIZE: u64 = 249_753_412; // Skyrim.esm, i.e. well past 32 bits.
        let mut buf = vec![0u8; 104];

        fill(CLASS_STANDARD, &mut buf, false, SIZE).unwrap();
        assert_eq!(i64_at(&buf, 0), SIZE as i64, "standard AllocationSize");
        assert_eq!(i64_at(&buf, 8), SIZE as i64, "standard EndOfFile");

        buf.iter_mut().for_each(|b| *b = 0);
        fill(CLASS_NETWORK_OPEN, &mut buf, false, SIZE).unwrap();
        assert_eq!(i64_at(&buf, 40), SIZE as i64, "network-open EndOfFile");

        for class in [CLASS_STAT, CLASS_STAT_BASIC] {
            buf.iter_mut().for_each(|b| *b = 0);
            fill(class, &mut buf, false, SIZE).unwrap();
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
            buf.iter_mut().for_each(|b| *b = 0);
            fill(class, &mut buf, true, 0).unwrap();
            assert_eq!(
                u32_at(&buf, attr_off) & FILE_ATTRIBUTE_DIRECTORY,
                FILE_ATTRIBUTE_DIRECTORY,
                "class {class} did not mark a directory"
            );

            buf.iter_mut().for_each(|b| *b = 0);
            fill(class, &mut buf, false, 1).unwrap();
            assert_eq!(
                u32_at(&buf, attr_off) & FILE_ATTRIBUTE_DIRECTORY,
                0,
                "class {class} marked a file as a directory"
            );
        }

        // FileStandardInformation carries a boolean rather than an attribute.
        buf.iter_mut().for_each(|b| *b = 0);
        fill(CLASS_STANDARD, &mut buf, true, 0).unwrap();
        assert_eq!(buf[21], 1, "standard Directory flag");
        buf.iter_mut().for_each(|b| *b = 0);
        fill(CLASS_STANDARD, &mut buf, false, 1).unwrap();
        assert_eq!(buf[21], 0, "standard Directory flag set for a file");
    }
}
