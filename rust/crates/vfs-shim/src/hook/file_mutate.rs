//! Delete and rename: `NtDeleteFile` and `NtSetInformationFile`.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{
    ENGINE, PATH_TABLE, TRAMP_DELETE, TRAMP_SETINFO, fuse_root_directory, in_hook_reenter,
    object_name_str, parse_rename_target, path_is_ours, path_of_handle, path_of_tracked,
    redirected_oa, to_nt_path,
};
use crate::ntdef::{
    FILE_DISPOSITION_DELETE, FILE_DISPOSITION_INFORMATION, FILE_DISPOSITION_INFORMATION_EX,
    FILE_END_OF_FILE_INFORMATION, FILE_POSITION_INFORMATION, FILE_RENAME_INFORMATION,
    FILE_RENAME_INFORMATION_EX, FileEndOfFileInformation, FilePositionInformation, NtDeleteFileFn,
    ObjectAttributes, STATUS_ACCESS_DENIED, STATUS_FILE_IS_A_DIRECTORY,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};
use crate::synth_file::FileView;
use core::ffi::c_void;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// `NtDeleteFile` hook: the **path-based** delete. It takes only an
/// `OBJECT_ATTRIBUTES`, so unhooked it would unlink the real file under a managed
/// root; there is no handle to be wrong about. The decision is made on the path
/// (`path_of_tracked`), in the order the open hooks use:
///
/// 1. `FuseClient::vpath_under_root`: if the director claims the path, its answer is
///    the caller's answer, both ways. It never continues to the kernel from here.
/// 2. `Engine::whiteout`: the shim-local overlay, which is also what `setinfo_hook`'s
///    non-synthetic branch does for a handle-based delete of the same path.
/// 3. `path_is_ours`: the backstop. Where the engine declines, an under-root path
///    fails closed with `STATUS_ACCESS_DENIED` (not the director's
///    `STATUS_UNSUCCESSFUL`: that one is "the graph said no", this is "nothing here
///    is willing to answer").
///
/// Outside every root the call is trampolined unchanged.
/// See docs/shim-invariants.md, "Sealed root: deletes and renames".
pub(super) unsafe fn delete_hook_body(oa: *const ObjectAttributes) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::DeleteFile);
    let tramp = match TRAMP_DELETE.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // Shim-initiated I/O (overlay writes, the panic log, copy-up) must reach
    // the real ntdll, exactly as in `create_hook`/`open_hook`.
    if in_hook_reenter() {
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(oa) };
    }
    // Decode once, and hold the `UncachedScope` for as long as this call
    // decides with the result — `vpath_under_root`, `whiteout` and
    // `path_is_ours` are all `RootMap`-backed and cached the same way
    // `decision_for` is. See `parent_dir_of_handle`'s case 4 and
    // `DecodedPath`'s doc comment.
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    let decoded = match unsafe { path_of_tracked(oa) } {
        Ok(d) => d,
        Err(st) => return st,
    };
    let _uncached_guard = decoded
        .as_ref()
        .is_some_and(|d| d.os_consulted)
        .then(vfs_redirect::UncachedScope::enter);
    let Some(path) = decoded.as_ref().map(|d| d.path.as_str()) else {
        // An undecodable delete is an undecodable open by another name: it
        // bypasses every decision we would have made. Recorded rather than
        // silently trampolined, so it shows up in the same place.
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        crate::hookstats::note_undecodable(unsafe { object_name_str(oa) }.as_deref());
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(oa) };
    };

    if let Some(client) = crate::director::global() {
        if let Some((root, vp)) = client.route(path) {
            let vp = vp.as_str();
            client.names_changed(root, vp);
            crate::read_cache::invalidate_path(root.0, vp);
            return match client.delete(root, vp) {
                Ok(()) => STATUS_SUCCESS,
                Err(st) => delete_status_for(st),
            };
        }
    }
    if let Some(engine) = ENGINE.get() {
        if engine.whiteout(path) {
            return STATUS_SUCCESS;
        }
    }
    if path_is_ours(path) {
        return STATUS_ACCESS_DENIED;
    }
    // Outside every root. A FUSE-synthetic `RootDirectory` is invalid to the
    // kernel even here, so rebuild the OA absolute rather than hand the
    // synthetic handle over — the same narrow disagreement case
    // `tramp_create_abs` documents.
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    if unsafe { fuse_root_directory(oa) } {
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        return unsafe { tramp_delete_abs(tramp, oa, path) };
    }
    // SAFETY: the original NT function, called with valid NT arguments.
    unsafe { tramp(oa) }
}

/// The NT status for a director `OP_DELETE` refusal. Flattening every refusal to
/// `STATUS_UNSUCCESSFUL` (`ERROR_GEN_FAILURE`) would stop delete-then-create
/// callers, which tolerate only `ERROR_FILE_NOT_FOUND`; this is the same mapping as
/// `try_fuse_create`'s `Err` arms, kept in one function so the two cannot drift:
///
/// - `ST_NOT_FOUND` -> `STATUS_OBJECT_NAME_NOT_FOUND`: nothing to delete is not a failure.
/// - `ST_READ_ONLY` -> `STATUS_ACCESS_DENIED`, as a read-only filesystem answers `DeleteFileW`.
/// - `ST_IS_DIR` -> `STATUS_FILE_IS_A_DIRECTORY`, folded to `ERROR_ACCESS_DENIED`, as
///   `DeleteFileW` answers a directory.
/// - Anything else (I/O error, a provider that broke) keeps `STATUS_UNSUCCESSFUL`.
fn delete_status_for(st: i32) -> NTSTATUS {
    match st {
        vfs_protocol::ST_NOT_FOUND => STATUS_OBJECT_NAME_NOT_FOUND,
        vfs_protocol::ST_READ_ONLY => STATUS_ACCESS_DENIED,
        vfs_protocol::ST_IS_DIR => STATUS_FILE_IS_A_DIRECTORY,
        _ => STATUS_UNSUCCESSFUL,
    }
}

/// `NtDeleteFile` via the trampoline with an absolute NT path and a **null**
/// `RootDirectory`. The `NtDeleteFile` counterpart of [`tramp_create_abs`];
/// see that function for when a synthetic root reaches a fall-through at all.
unsafe fn tramp_delete_abs(
    tramp: NtDeleteFileFn,
    oa: *const ObjectAttributes,
    abs_path: &str,
) -> NTSTATUS {
    let nt = to_nt_path(abs_path);
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    let new_oa = match unsafe { redirected_oa(oa, &nt) } {
        Ok(o) => o,
        Err(st) => return st,
    };
    // SAFETY: the original NT function, called with valid NT arguments.
    unsafe { tramp(new_oa.as_ptr()) }
}

/// True when this `NtSetInformationFile` call requests a delete (either
/// disposition class with the delete flag/boolean set).
unsafe fn is_delete_request(info: *mut c_void, length: u32, class: u32) -> bool {
    !info.is_null()
        && match class {
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            FILE_DISPOSITION_INFORMATION => unsafe { length >= 1 && *(info as *const u8) != 0 },
            FILE_DISPOSITION_INFORMATION_EX => {
                length >= 4
                    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
                    && unsafe { core::ptr::read_unaligned(info as *const u32) } & FILE_DISPOSITION_DELETE != 0
            }
            _ => false,
        }
}

/// The NT path a handle-based delete/rename should act on, and whether finding it
/// required consulting the OS about the handle's *current* target (the provenance
/// bit [`DecodedPath`] carries, for the same reason).
///
/// The order is correctness: the recorded name wins wherever there is one, because
/// for a redirected handle it is the virtual path while the handle itself targets
/// the overlay copy.
///
/// 1. `PATH_TABLE`: an intercepted open whose path was under a managed root.
/// 2. `HANDLE_PATHS`: every other intercepted open.
/// 3. `GetFinalPathNameByHandleW`: a handle the shim never saw opened (inherited,
///    duplicated in, or opened before injection). Without this a `PATH_TABLE` miss
///    read as "nothing to do" and a delete reached the real file under a root.
///
/// See docs/shim-invariants.md, "Sealed root: deletes and renames".
///
/// # Safety
/// `handle` must be the live handle of an in-flight `NtSetInformationFile` this
/// process is making, which is what `final_path_for_handle` requires. This
/// neither closes it nor takes ownership of it.
unsafe fn setinfo_source_path(handle: HANDLE) -> Option<(String, bool)> {
    if let Ok(t) = PATH_TABLE.lock() {
        if let Some(p) = t.get(&(handle as isize)) {
            return Some((p.clone(), false));
        }
    }
    if let Some(p) = path_of_handle(handle) {
        return Some((p, false));
    }
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    unsafe { vfs_win::final_path_for_handle(handle).map(|p| (p, true)) }
}

/// `FileCompletionInformation` — binds a handle to an I/O completion port.
const FILE_COMPLETION_INFORMATION: u32 = 30;

/// `NtSetInformationFile` hook. For director FUSE (pure-ring) virtual handles it
/// routes truncate (`FileEndOfFileInformation`), delete, and rename to the director
/// overlay over the ring. For legacy local-overlay handles it converts a delete
/// or rename of a tracked under-root handle into an overlay whiteout/rename and
/// suppresses the real operation, so the mod backing / real file is preserved
/// but the path reads as gone/moved.
///
/// Two checks sit on top of that, each commented where it is made: the source is
/// resolved even when no table knows the handle (`setinfo_source_path`), and a
/// rename is refused on its *target* as well as its source. The rule they leave is
/// one sentence: a rename either has both sides under the same root, and is
/// routed, or it touches no root at all, and passes through. Everything else is
/// refused, and a delete of an under-root path that nothing here absorbed is
/// refused with it rather than reaching the kernel.
/// See docs/shim-invariants.md, "Sealed root: deletes and renames".
pub(super) unsafe fn setinfo_hook_body(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::SetInfo);
    let tramp = match TRAMP_SETINFO.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::synth_file::is_fuse_synth(handle as isize) {
        if class == FILE_COMPLETION_INFORMATION {
            // Binding a synthetic handle to a completion port: the kernel will
            // never post a packet for it, so any caller waiting on that port
            // for this handle waits forever.
            crate::hookstats::note_iocp_bind();
        }
        if class == FILE_POSITION_INFORMATION
            && !info.is_null()
            && length as usize >= core::mem::size_of::<FilePositionInformation>()
        {
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            let pos = unsafe { (*(info as *const FilePositionInformation)).current_byte_offset };
            if pos >= 0 {
                crate::synth_file::set_position(handle as isize, pos as u64);
            }
            return STATUS_SUCCESS;
        }
        // Truncate (`File::set_len`) → ring OP_SETATTR on the virtual write handle.
        if class == FILE_END_OF_FILE_INFORMATION
            && !info.is_null()
            && length as usize >= core::mem::size_of::<FileEndOfFileInformation>()
        {
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            let eof = unsafe { (*(info as *const FileEndOfFileInformation)).end_of_file };
            if let Some(f) = crate::synth_file::cache(handle as isize) {
                crate::read_cache::invalidate(&f);
            }
            if let (Some(FileView { fh, .. }), Some(c)) = (
                crate::synth_file::lookup(handle as isize),
                crate::director::global(),
            ) {
                if eof >= 0 && c.truncate(fh, eof as u64).is_ok() {
                    crate::synth_file::set_size(handle as isize, eof as u64);
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0) };
                    return STATUS_SUCCESS;
                }
            }
            return STATUS_UNSUCCESSFUL;
        }
        // Delete / rename of a virtual handle → ring OP_DELETE / OP_RENAME, keyed
        // by the NT path recorded (record_path) when the handle was opened.
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        let is_delete = unsafe { is_delete_request(info, length, class) };
        let is_rename = matches!(class, FILE_RENAME_INFORMATION | FILE_RENAME_INFORMATION_EX);
        if is_delete || is_rename {
            let nt = match PATH_TABLE.lock() {
                Ok(t) => t.get(&(handle as isize)).cloned(),
                Err(_) => None,
            };
            if let (Some(nt), Some(c)) = (nt, crate::director::global()) {
                if let Some((root, src)) = c.route(&nt) {
                    c.names_changed(root, &src);
                    crate::read_cache::invalidate_path(root.0, &src);
                    let ok = if is_delete {
                        c.delete(root, &src).is_ok()
                    } else {
                        // The destination is a name being created, so it goes
                        // as the caller spelled it — which is also how a
                        // rename that changes only the letter case says what
                        // the new case is. See `FuseClient::vpath_as_spelled`.
                        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                        let target = unsafe { parse_rename_target(info, length) };
                        match target.as_deref().and_then(|t| c.route_as_spelled(t)) {
                            // A rename whose target lands under a *different* root is refused rather than
                            // guessed at: the wire carries one root for both sides and the provider contract
                            // has no cross-root move. It does not fall through: `ok = false`, then
                            // `STATUS_UNSUCCESSFUL` below, and the engine branch fails closed the same way.
                            Some((dst_root, dst)) if dst_root == root => {
                                c.names_changed(root, &vfs_core::fold(&dst));
                                crate::read_cache::invalidate_path(root.0, &vfs_core::fold(&dst));
                                let renamed = c.rename(root, &src, &dst).is_ok();
                                if renamed {
                                    // The handle follows the file: what it is
                                    // finally named, and its id, are the new
                                    // path's from here on.
                                    if let Some(t) = target {
                                        let nt = to_nt_path(&t);
                                        if let Ok(mut table) = PATH_TABLE.lock() {
                                            table.insert(handle as isize, nt.clone());
                                        }
                                        crate::synth_file::set_abs_path(handle as isize, nt);
                                    }
                                }
                                renamed
                            }
                            _ => false,
                        }
                    };
                    if ok {
                        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                        unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0) };
                        return STATUS_SUCCESS;
                    }
                    return STATUS_UNSUCCESSFUL;
                }
                // is_delete/is_rename matched the class but the handle's path
                // or vpath could not be resolved — falls through to the soft
                // no-op below rather than a hard failure. Still worth logging:
                // it means a delete/rename was silently swallowed.
            }
        }
        // Everything else lands here: a class we deliberately never act on
        // (or a delete/rename we recognized but could not route). Silent
        // success here for a class we actually needed to handle is exactly
        // the bug this counter exists to make discoverable — see
        // `hookstats::note_setinfo_noop`.
        crate::hookstats::note_setinfo_noop(class);
        return STATUS_SUCCESS;
    }
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    let is_delete = unsafe { is_delete_request(info, length, class) };
    let is_rename = matches!(class, FILE_RENAME_INFORMATION | FILE_RENAME_INFORMATION_EX);

    if is_delete || is_rename {
        // Not just `PATH_TABLE`: a handle the shim never saw opened has no
        // entry there, and reading that miss as "not ours" is what let a
        // delete on an inherited or pre-injection under-root handle reach the
        // real file. See `setinfo_source_path`.
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        let source = unsafe { setinfo_source_path(handle) };
        // Held for every `RootMap`-backed question asked with an OS-consulted
        // source path below (`Engine::whiteout`/`rename`, `path_is_ours`) —
        // that string is a fact about the handle's target right now, not a
        // pure function of its own bytes. See `vfs_redirect::UncachedScope`.
        let _uncached_guard = source
            .as_ref()
            .is_some_and(|(_, os_consulted)| *os_consulted)
            .then(vfs_redirect::UncachedScope::enter);
        let nt = source.map(|(p, _)| p);
        if let (Some(nt), Some(engine)) = (nt, ENGINE.get()) {
            let handled = if is_delete {
                engine.whiteout(&nt)
            } else {
                // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                match unsafe { parse_rename_target(info, length) } {
                    Some(target) => match engine.rename(&nt, &target) {
                        crate::engine::RenameOutcome::Handled => true,
                        // Both sides under managed roots, but different ones.
                        // Trampolining here is what let the kernel physically
                        // move an overlay-captured file out onto real disk
                        // under the destination root — where it then reads
                        // back as missing, because that root seals anything
                        // the provider graph does not serve. Fail closed, with
                        // the same status the FUSE-handle branch above already
                        // returns for the identical case.
                        crate::engine::RenameOutcome::CrossRoot => return STATUS_UNSUCCESSFUL,
                        crate::engine::RenameOutcome::Declined => false,
                    },
                    None => false,
                }
            };
            if handled {
                // Suppress the real delete/rename; report success to the caller.
                // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0) };
                return STATUS_SUCCESS;
            }
            // The source is under a managed root and nothing above absorbed the operation, so
            // `tramp` would hand it to the kernel, which acts on the real file. Three routes
            // end here: a delete `Engine::whiteout` declined (as `delete_hook` already
            // refuses), a rename out of the root to a target outside every root, and a rename
            // target that cannot be parsed. The rule: a rename either has both sides under the
            // same root and is routed, or does not touch a root at all and is trampolined;
            // everything between is refused.
            // See docs/shim-invariants.md, "Sealed root: deletes and renames".
            if path_is_ours(&nt) {
                return STATUS_ACCESS_DENIED;
            }
        }
        // A rename whose *target* lands under a managed root. The arms above are keyed on
        // the source, and for a source outside every root none of them runs, so `tramp`
        // would physically create a file under a root that seals everything the provider
        // graph does not serve. Refused with `STATUS_ACCESS_DENIED`; the cross-root arm's
        // `STATUS_UNSUCCESSFUL` is a different answer ("the graph cannot express this
        // move", against "the destination will not accept content by this route").
        // `OP_RENAME` carries one root, so there is no import operation to route it to.
        //
        // NOTE: `parse_rename_target` discards `parent_dir_of_handle`'s OS-consulted
        // provenance bit, so a target named against a directory handle the shim never saw
        // opened reaches `path_is_ours` here without an `UncachedScope`. That is the known
        // gap `parse_rename_target` already records for `engine.rename`; both callers are
        // fixed at once there.
        // See docs/shim-invariants.md, "Sealed root: deletes and renames".
        if is_rename {
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            if let Some(target) = unsafe { parse_rename_target(info, length) } {
                if path_is_ours(&target) {
                    return STATUS_ACCESS_DENIED;
                }
            }
        }
    }
    // SAFETY: the original NT function, called with valid NT arguments.
    unsafe { tramp(handle, iosb, info, length, class) }
}
