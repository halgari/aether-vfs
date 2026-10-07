//! Attribute queries by path: `NtQueryAttributesFile`, `NtQueryFullAttributesFile`, `NtQueryInformationByName`.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{
    BASIC_LEN, ENGINE, NETWORK_OPEN_LEN, TRAMP_QATTR, TRAMP_QFULL, TRAMP_QIBN,
    allow_disk_fallthrough, attributes, caller_buf, in_hook_reenter, path_file_id, path_of,
    put_basic, put_network_open,
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
                let buf = unsafe { caller_buf(info, length as usize) };
                let Some(n) = vfs_ntlayout::by_name_info(class_raw, buf, st.is_dir, st.size) else {
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
                    let buf = unsafe { caller_buf(info.cast(), BASIC_LEN) };
                    put_basic(buf, SYNTH_FILETIME, attributes(st.is_dir));
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
                    let buf = unsafe { caller_buf(info.cast(), NETWORK_OPEN_LEN) };
                    put_network_open(buf, SYNTH_FILETIME, st.size, attributes(st.is_dir));
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
