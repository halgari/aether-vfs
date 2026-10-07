//! Directory enumeration: `NtQueryDirectoryFile` and `NtQueryDirectoryFileEx`.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{HANDLES, TRAMP_QDIR, TRAMP_QDIREX, path_of_handle, strip_whiteout_markers};
use crate::ntdef::{
    SL_RESTART_SCAN, SL_RETURN_SINGLE_ENTRY, STATUS_BUFFER_OVERFLOW, STATUS_NO_MORE_FILES,
    STATUS_SUCCESS, STATUS_UNSUCCESSFUL, UnicodeString,
};
use core::ffi::c_void;
use vfs_ntlayout::{DirInfoClass, DirItem, DirStatus, write_dir_info};
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// Per-handle enumeration cursor over a built directory listing.
///
/// The field was `merged` when a listing really was a merge of the real
/// directory with a snapshot or overlay. Nothing merges any more: under a
/// managed root this is the director's own `readdir`, whole and unaltered
/// (see `serve_dir_query`), and a directory outside every root never gets an
/// `EnumState` at all — the OS answers it directly.
pub(super) struct EnumState {
    pub(super) entries: Vec<DirItem>,
    cursor: usize,
}

/// A tracked directory handle: the NT path it was opened as, and its lazily
/// built enumeration state (rebuilt on `SL_RESTART_SCAN`).
pub(super) struct DirTracked {
    pub(super) dir_nt_path: String,
    pub(super) state: Option<EnumState>,
}

/// Extract a search wildcard from a `PUNICODE_STRING`. Null/empty/`*`/`*.*`
/// mean "match everything" (`Ok(None)`). A string `ntbuf::us_units` rejects is `Err`.
unsafe fn wildcard_of(file_name: *const UnicodeString) -> Result<Option<String>, NTSTATUS> {
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    Ok(unsafe { crate::ntbuf::us_string(file_name) }?
        .filter(|s| !(s.is_empty() || s == "*" || s == "*.*")))
}

#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn qdirex_hook_body(
    handle: HANDLE,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class_raw: u32,
    flags: u32,
    file_name: *const UnicodeString,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QDirEx);
    let tramp = match TRAMP_QDIREX.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    unsafe {
        serve_dir_query(
            handle,
            iosb,
            info,
            length,
            class_raw,
            flags & SL_RESTART_SCAN != 0,
            flags & SL_RETURN_SINGLE_ENTRY != 0,
            file_name,
            &|| {
                tramp(
                    handle, event, apc, apc_ctx, iosb, info, length, class_raw, flags, file_name,
                )
            },
        )
    }
}

/// The classic entry point. Same body, different argument shape.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn qdir_hook_body(
    handle: HANDLE,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class_raw: u32,
    single: u8,
    file_name: *const UnicodeString,
    restart: u8,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QDir);
    let tramp = match TRAMP_QDIR.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    unsafe {
        serve_dir_query(
            handle,
            iosb,
            info,
            length,
            class_raw,
            restart != 0,
            single != 0,
            file_name,
            &|| {
                tramp(
                    handle, event, apc, apc_ctx, iosb, info, length, class_raw, single, file_name,
                    restart,
                )
            },
        )
    }
}

/// Shared body for both enumeration entry points.
#[allow(clippy::too_many_arguments)]
unsafe fn serve_dir_query(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class_raw: u32,
    restart: bool,
    single: bool,
    file_name: *const UnicodeString,
    passthrough: &dyn Fn() -> NTSTATUS,
) -> NTSTATUS {
    // Unknown info class -> let the OS handle it verbatim.
    let class = match DirInfoClass::from_u32(class_raw) {
        Some(c) => c,
        None => return passthrough(),
    };
    let key = handle as isize;

    // Phase 1 (locked): is this a tracked handle, and must we (re)build?
    let (need_build, dir_path) = {
        let table = match HANDLES.lock() {
            Ok(t) => t,
            Err(_) => return passthrough(),
        };
        match table.dir(key) {
            None => {
                drop(table);
                // Untracked: a directory outside the managed root, so the OS
                // answers. Worth recording anyway — "the game listed a Data
                // that isn't ours" is the diagnosis for an empty load order.
                if crate::hookstats::enabled() {
                    let dir = path_of_handle(handle).unwrap_or_else(|| "<unknown>".to_string());
                    crate::hookstats::note_readdir(
                        &dir,
                        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                        unsafe { wildcard_of(file_name) }.ok().flatten().as_deref(),
                        0,
                        crate::hookstats::ReadDirSource::Os,
                    );
                }
                return passthrough();
            }
            Some(t) => (restart || t.state.is_none(), t.dir_nt_path.clone()),
        }
    };

    // Phase 2 (unlocked): build the listing. Every listing built here is under a
    // managed root, so it may hold only what the director serves (its `readdir`, whole
    // and unmerged). The real directory behind the mount is never read into one. A
    // tracked directory the client does not route gets an empty listing: not reached
    // (a directory is tracked only when `path_is_ours`, the same question), and kept so
    // that a drifted predicate fails closed.
    // See docs/shim-invariants.md, "Enumeration containment".
    //
    // The ring round trip calls out, so the lock must NOT be held here (NtClose also
    // takes it).
    let rebuilt = if need_build {
        // A wildcard NT's own capture refuses gets NT's answer.
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        let wildcard = match unsafe { wildcard_of(file_name) } {
            Ok(w) => w,
            Err(st) => return st,
        };
        let routed = crate::director::global().and_then(|c| c.route(&dir_path).map(|hit| (c, hit)));
        match routed {
            Some((client, (root, vp))) => {
                let vp = vp.as_str();
                let items = match client.readdir(root, vp) {
                    Ok(entries) => {
                        let items: Vec<DirItem> = entries
                            .into_iter()
                            .map(|e| DirItem {
                                name: e.name,
                                is_dir: e.is_dir,
                                size: e.size,
                                mtime: e.mtime,
                            })
                            .collect();
                        // The director's write layer is the directory older shims wrote their own
                        // `<name>.__vfs_wh__` whiteout markers into, and it spells whiteouts
                        // `.wh.<name>`, so those markers come back as ordinary files. Strip them
                        // before the wildcard filter (`strip_whiteout_markers`).
                        // See docs/shim-invariants.md, "Enumeration containment".
                        let mut items = strip_whiteout_markers(items);
                        if let Some(ref w) = wildcard {
                            items.retain(|i| {
                                vfs_core::wildcard_match(w, &i.name)
                                    || i.name.eq_ignore_ascii_case(w)
                            });
                        }
                        items
                    }
                    Err(_) => Vec::new(),
                };
                // Not fixed here: a marker still does not hide its target from an open through
                // the director. See docs/shim-invariants.md, "Enumeration containment".
                Some((items, crate::hookstats::ReadDirSource::Director))
            }
            None => Some((
                Vec::new(),
                crate::hookstats::ReadDirSource::ContainedNoDirector,
            )),
        }
    } else {
        None
    };

    // Phase 3 (locked): store the built listing (if rebuilt) and serve a slice.
    //
    // The caller's buffer is filled after the guard is released, never under it:
    // `write_dir_info` writes into a scratch buffer we own and the copy into `info`
    // happens below, unlocked. `info` may lie in one of our own demand-paged regions;
    // touching it can fault into `lazy_section`, whose file I/O re-enters the shim
    // through `NtClose` and takes the handle table again, and `std::sync::Mutex` is not
    // reentrant. A scratch buffer rather than cloned entries: the copy is bounded by
    // `length`, a listing is not.
    // See docs/shim-invariants.md, "Enumeration containment".
    let mut scratch = vec![0u8; length as usize];
    let result = {
        let mut table = match HANDLES.lock() {
            Ok(t) => t,
            Err(_) => return passthrough(),
        };
        let tracked = match table.dir_mut(key) {
            Some(t) => t,
            None => return passthrough(),
        };
        if let Some((entries, source)) = rebuilt {
            crate::hookstats::note_readdir(
                &dir_path,
                // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                unsafe { wildcard_of(file_name) }.ok().flatten().as_deref(),
                entries.len(),
                source,
            );
            tracked.state = Some(EnumState { entries, cursor: 0 });
        }
        let st = match tracked.state.as_mut() {
            Some(s) => s,
            None => return passthrough(),
        };
        let result = write_dir_info(class, &st.entries[st.cursor..], &mut scratch, single);
        st.cursor += result.count;
        result
    };

    // Unlocked from here: a fault on `info` can now re-enter the shim freely.
    if result.bytes > 0 {
        // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
        let buf = unsafe { core::slice::from_raw_parts_mut(info as *mut u8, length as usize) };
        buf[..result.bytes].copy_from_slice(&scratch[..result.bytes]);
    }

    let status = match result.status {
        DirStatus::Success => STATUS_SUCCESS,
        DirStatus::NoMoreFiles => STATUS_NO_MORE_FILES,
        DirStatus::BufferOverflow => STATUS_BUFFER_OVERFLOW,
    };
    // IO_STATUS_BLOCK: Status (NTSTATUS) @0, Information (ULONG_PTR) @8.
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    unsafe { crate::ntbuf::iosb_set(iosb, status, result.bytes) };
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::test_support::us_raw;
    use crate::ntdef::STATUS_OBJECT_NAME_INVALID;

    /// `wildcard_of`: `*` and `*.*` and the empty string mean everything; a string NT's capture
    /// would reject (odd length, NULL buffer with a length) is an `Err`, where the odd one used
    /// to be a truncated pattern and the NULL one used to mean everything.
    #[test]
    fn wildcard_of_follows_the_shared_unicode_string_rule() {
        let enc = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
        let mut star = enc("*");
        let mut stardot = enc("*.*");
        let mut pat = enc("a*.esp");
        unsafe {
            assert_eq!(wildcard_of(core::ptr::null()), Ok(None));
            assert_eq!(wildcard_of(&us_raw(2, star.as_mut_ptr())), Ok(None));
            assert_eq!(wildcard_of(&us_raw(6, stardot.as_mut_ptr())), Ok(None));
            assert_eq!(wildcard_of(&us_raw(0, core::ptr::null_mut())), Ok(None));
            assert_eq!(
                wildcard_of(&us_raw(12, pat.as_mut_ptr())),
                Ok(Some("a*.esp".to_string()))
            );
            assert_eq!(
                wildcard_of(&us_raw(11, pat.as_mut_ptr())),
                Err(STATUS_OBJECT_NAME_INVALID)
            );
            assert_eq!(
                wildcard_of(&us_raw(4, core::ptr::null_mut())),
                Err(crate::ntdef::STATUS_ACCESS_VIOLATION)
            );
        }
    }
}
