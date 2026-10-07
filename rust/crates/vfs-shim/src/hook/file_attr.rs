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

/// What the stat-by-path routine found out about a path.
struct PathStat {
    is_dir: bool,
    size: u64,
}

/// Who answered a stat: the director, or the shim-local write overlay.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StatSource {
    Director,
    Overlay,
}

/// What a stat-by-path call saw, for the hook's `note_stat` label.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StatEvent {
    /// The path is outside every root.
    Outside,
    /// The director has it.
    Found,
    /// The director says it is not there.
    NotFound,
    /// The director failed.
    Failed,
}

/// The `note_stat` label `NtQueryAttributesFile` and `NtQueryFullAttributesFile` give an event.
fn plain_label(event: StatEvent) -> &'static str {
    match event {
        StatEvent::Outside => "outside-root",
        StatEvent::Found => "found",
        StatEvent::NotFound => "NOT-FOUND",
        StatEvent::Failed => "ERROR",
    }
}

/// Stat a path for a hook, with no handle anywhere in the call: the director first, then the
/// shim-local write overlay (which holds content the director never sees: a just-created or
/// modified file, a runtime delete's whiteout).
///
/// - `note` is told what the director said, for the hook's `note_stat` label.
/// - `fill` writes the answer into the caller's buffer and returns whether it could. A director
///   answer that does not fit goes on to the overlay, then to the real call.
/// - A path under a root that the director does not have is sealed (`STATUS_OBJECT_NAME_NOT_FOUND`)
///   unless disk fall-through is on (`allow_disk_fallthrough`), and a director failure is
///   `STATUS_UNSUCCESSFUL`: under a root the real tree is never consulted.
///
/// `Some(status)`: the hook returns it. `None`: the hook makes the real call.
fn stat_by_path(
    path: &str,
    mut note: impl FnMut(StatEvent),
    mut fill: impl FnMut(&PathStat, StatSource) -> bool,
) -> Option<NTSTATUS> {
    // SAFETY: `fuse_path_attr` takes no pointer; it asks the director about a path.
    match unsafe { fuse_path_attr(path) } {
        None => note(StatEvent::Outside),
        Some(Ok((is_dir, size, _mtime))) => {
            note(StatEvent::Found);
            if fill(&PathStat { is_dir, size }, StatSource::Director) {
                return Some(STATUS_SUCCESS);
            }
        }
        Some(Err(st)) if st == vfs_protocol::ST_NOT_FOUND => {
            note(StatEvent::NotFound);
            if !allow_disk_fallthrough() {
                return Some(STATUS_OBJECT_NAME_NOT_FOUND);
            }
        }
        Some(Err(_)) => {
            note(StatEvent::Failed);
            return Some(STATUS_UNSUCCESSFUL);
        }
    }
    // The director already had first refusal; the overlay is the only thing left that can
    // answer without it.
    if let Some(engine) = ENGINE.get() {
        match engine.overlay_state(path) {
            Some(OverlayState::Present { is_dir, size, .. }) => {
                if fill(&PathStat { is_dir, size }, StatSource::Overlay) {
                    return Some(STATUS_SUCCESS);
                }
            }
            Some(OverlayState::Whiteout) => return Some(STATUS_OBJECT_NAME_NOT_FOUND),
            Some(OverlayState::Absent) | None => {}
        }
    }
    None
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
        let answer = stat_by_path(
            &path,
            |event| match event {
                // Logged too: a stat that lands outside the root is exactly how a
                // wrong Data directory would present, and it is otherwise silent.
                StatEvent::Outside => {
                    crate::hookstats::note_stat(&path, &format!("byname{class_raw}-outside"))
                }
                StatEvent::NotFound => {
                    crate::hookstats::note_stat(&path, &format!("byname{class_raw}-missing"))
                }
                StatEvent::Found | StatEvent::Failed => {}
            },
            |st, source| {
                // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                let Some(n) =
                    (unsafe { fill_by_name(class_raw, info, length, st.is_dir, st.size) })
                else {
                    if source == StatSource::Director {
                        crate::hookstats::note_stat(&path, &format!("byname{class_raw}-UNSUP"));
                    }
                    return false;
                };
                if source == StatSource::Director {
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
                }
                // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, n) };
                true
            },
        );
        if let Some(st) = answer {
            return st;
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
        let answer = stat_by_path(
            &path,
            |event| crate::hookstats::note_stat(&path, plain_label(event)),
            |st, _| {
                if !info.is_null() {
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    unsafe { put_basic(info as *mut u8, SYNTH_FILETIME, attributes(st.is_dir)) };
                }
                true
            },
        );
        if let Some(st) = answer {
            return st;
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
        let answer = stat_by_path(
            &path,
            |event| crate::hookstats::note_stat(&path, plain_label(event)),
            |st, _| {
                if !info.is_null() {
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    unsafe {
                        put_network_open(
                            info as *mut u8,
                            SYNTH_FILETIME,
                            st.size,
                            attributes(st.is_dir),
                        )
                    };
                }
                true
            },
        );
        if let Some(st) = answer {
            return st;
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
