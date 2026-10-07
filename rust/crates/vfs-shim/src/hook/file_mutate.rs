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
use core::ffi::c_void;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// `NtDeleteFile` hook — the **path-based** delete (gate 5, Task 5).
///
/// **This was the one unhooked NT API that reached real disk.** The others on
/// this project's list all take a handle, so leaving one unhooked fails safely:
/// under a managed root the caller is holding a synthetic handle, the kernel
/// does not own it, and the call comes back `STATUS_INVALID_HANDLE`. This one
/// takes only an `OBJECT_ATTRIBUTES`. There is no handle to be wrong about, so
/// an unhooked call resolves the path itself and unlinks the real file sitting
/// under the root — which is the exact thing the root's contract says is
/// unreachable.
///
/// **The decision is made on the path, like `create_hook`/`open_hook`, and by
/// the same machinery.** `path_of_tracked` decodes the `OBJECT_ATTRIBUTES`
/// (including a handle-relative name, and including the FUSE-synthetic
/// `RootDirectory` a virtual directory handle produces), and the three
/// questions asked of that path below are the three the open hooks already ask,
/// in the same order:
///
/// 1. `FuseClient::vpath_under_root` — the director's own notion of the root.
///    If it claims the path, the director's answer is the caller's answer, both
///    ways: `OP_DELETE` accepted is `STATUS_SUCCESS`, `OP_DELETE` refused is a
///    failure the caller sees. It never continues to the kernel from here, for
///    the same reason `try_fuse_create` does not: a refusal that falls through
///    is not a refusal.
/// 2. `Engine::whiteout` — the shim-local overlay, which is what
///    `setinfo_hook`'s non-synthetic branch already does for a *handle*-based
///    delete of the same path. Live when the director is absent (a `FuseClient`
///    that failed to attach still leaves an `Engine` with every declared root
///    and its overlay), and having both delete routes answer through the same
///    call is the point — two predicates that disagree about one path is the
///    failure mode this project has paid for twice.
/// 3. `path_is_ours` — the backstop. `Engine::whiteout` answers `false` for a
///    path it resolves with an *empty* remainder (the root directory itself)
///    and for an engine with no overlay at all, and `false` there must not mean
///    "let the kernel have it". Under a managed root that is the escape, not a
///    fallback, so it fails closed with `STATUS_ACCESS_DENIED` — distinct from
///    the director's own `STATUS_UNSUCCESSFUL` refusal above, because these are
///    different answers: one is "the graph said no", the other is "nothing here
///    is willing to answer, and the real file is not on offer".
///
/// Outside every root the call is trampolined unchanged, which is most of them
/// — a hook with an opinion about every delete in the process would break the
/// rest of it.
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

    if let Some(client) = crate::fuse_client::global() {
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

/// The NT status for a director `OP_DELETE` refusal.
///
/// **Flattening every refusal to `STATUS_UNSUCCESSFUL` is not neutral.** That
/// maps to `ERROR_GEN_FAILURE`, and the delete-then-create idiom — the single
/// most common thing callers do with a delete — treats only
/// `ERROR_FILE_NOT_FOUND` as benign and gives up on anything else. So a delete
/// of a path the director simply does not have would stop callers that a real
/// filesystem lets straight through. The open path already distinguishes these
/// (`try_fuse_create`'s `Err` arms); this is the same mapping for the same
/// reason, kept as one function so the two cannot drift:
///
/// - `ST_NOT_FOUND` -> `STATUS_OBJECT_NAME_NOT_FOUND` (`ERROR_FILE_NOT_FOUND`).
///   Nothing to delete is not a failure to delete.
/// - `ST_READ_ONLY` -> `STATUS_ACCESS_DENIED`. The director's own policy status
///   for "no `ReadWrite` provider serves this path", and `ERROR_ACCESS_DENIED`
///   is what a real read-only filesystem answers a `DeleteFileW`.
/// - `ST_IS_DIR` -> `STATUS_FILE_IS_A_DIRECTORY`, which `RtlNtStatusToDosError`
///   folds to `ERROR_ACCESS_DENIED` — exactly what `DeleteFileW` returns when
///   the name is a directory.
/// - Anything else (I/O error, a provider that broke) is a genuine failure and
///   keeps `STATUS_UNSUCCESSFUL`.
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

/// The NT path a handle-based delete/rename should act on, and whether finding
/// it required consulting the OS about the handle's *current* target (the same
/// provenance bit [`DecodedPath`] carries, for the same reason: a
/// `RootMap`-backed answer computed from it must not be cached under it).
///
/// **The third source is what closes an escape.** `PATH_TABLE` is populated by
/// `record_path`, which runs only on an open *this shim intercepted*. A handle
/// inherited across `CreateProcess`, duplicated in from another process, or
/// opened before injection is in no table of ours — and `setinfo_hook`'s
/// non-synthetic branch used to read that miss as "nothing to do" and hand the
/// call to the real `NtSetInformationFile`. For a **delete** that unlinked the
/// real file under a managed root, which is the same breach `delete_hook`
/// exists to prevent, arriving by a different door. There is no cheap
/// backstop for it either: a `PATH_TABLE` miss leaves no path at all, so there
/// is nothing to apply `path_is_ours` to until one is recovered.
///
/// So ask the OS, exactly as `parent_dir_of_handle`'s case 4 already does for
/// `OBJECT_ATTRIBUTES.RootDirectory` — `GetFinalPathNameByHandleW` needs no
/// reopen, since the caller is handing us a handle it currently holds.
///
/// **The order is correctness, not only cost.** `PATH_TABLE` holds the path the
/// caller *named*, and for a handle that came off `create_hook`'s
/// `Decision::Redirect` arm that is the virtual path while the handle itself
/// targets the overlay copy. Asking the OS first would hand back the overlay
/// file's own location, which resolves under no managed root, so the whiteout
/// would be skipped and the operation would go to the kernel — reintroducing the
/// escape from the other end. The recorded name wins wherever there is one:
///
/// 1. `PATH_TABLE` — an intercepted open whose path was under a managed root.
/// 2. `HANDLE_PATHS` — every other intercepted open. A handle here but not in
///    (1) is one `record_path` declined, i.e. outside every root, so this
///    answers the common "delete a file that is none of our business" case
///    without touching the OS.
/// 3. `GetFinalPathNameByHandleW`. Only reached for a handle the shim never saw
///    opened, and only on a delete/rename set-info, which is rare — this is not
///    a per-call cost on any hot path.
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
/// Two things sit on top of that, both from gate 5's Task 5, and each has its
/// own comment at the check itself:
///
/// - **The source is resolved even when no table knows the handle**
///   (`setinfo_source_path`). A `PATH_TABLE` miss used to mean "not ours", and
///   for an inherited or pre-injection handle on an under-root path that sent
///   a delete to the real file.
/// - **A refusal keyed on the *target*, not the source.** A rename whose
///   destination lands under a managed root is refused even when the source is
///   outside every one of them and no source-keyed arm ever looked at it.
///
/// Between them the rule is one sentence: a rename either has both sides under
/// the same root, and is routed, or it touches no root at all, and passes
/// through. Everything else is refused, and a delete of an under-root path that
/// nothing here absorbed is refused with it rather than reaching the kernel.
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
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
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
                crate::fuse_synth::set_position(handle as isize, pos as u64);
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
            if let Some(f) = crate::fuse_synth::cache(handle as isize) {
                crate::read_cache::invalidate(&f);
            }
            if let (Some((fh, _, _, _, _)), Some(c)) = (
                crate::fuse_synth::lookup(handle as isize),
                crate::fuse_client::global(),
            ) {
                if eof >= 0 && c.truncate(fh, eof as u64).is_ok() {
                    crate::fuse_synth::set_size(handle as isize, eof as u64);
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
            if let (Some(nt), Some(c)) = (nt, crate::fuse_client::global()) {
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
                            // A rename whose target lands under a *different*
                            // root is refused rather than guessed at: the
                            // wire carries one root for both sides, and the
                            // provider contract has no cross-root move.
                            //
                            // It does **not** fall through — an earlier
                            // version of this comment claimed it did, and
                            // `Engine::rename` was written to match that
                            // description, which is how the engine-side
                            // branch below ended up handing cross-root moves
                            // to the real filesystem. What actually happens is
                            // `ok = false` and `STATUS_UNSUCCESSFUL` twelve
                            // lines down, the same as any other refused
                            // delete/rename on a virtual handle. The engine
                            // branch now fails closed the same way.
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
                                        crate::fuse_synth::set_abs_path(handle as isize, nt);
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
            // **The source is under a managed root and nothing above absorbed
            // the operation.** `tramp` below would hand it to the kernel,
            // which acts on the real file — and this arm is reached by three
            // routes that all end that way:
            //
            //  - A **delete** that `Engine::whiteout` declined (no overlay
            //    configured, or a path resolving with an empty remainder).
            //    The path-based `delete_hook` has had a `path_is_ours`
            //    backstop for exactly this since it was written; leaving its
            //    sibling fail-open is the same divergence, and the next reader
            //    would have had two deletes to copy from and no way to tell
            //    which was right.
            //  - A **rename out** of a managed root to a target outside every
            //    one of them. `Engine::rename` answers `Declined` (its `to`
            //    side resolves nowhere) and the kernel then performs the move,
            //    which *unlinks a real file under a managed root*. That the
            //    destination is legitimately outside does not make the source
            //    side any less of a breach, and it is the same one the
            //    target-keyed check below closes in the other direction.
            //  - A **rename whose target cannot be parsed at all**
            //    (`parse_rename_target` -> `None`, e.g. a target named against
            //    a directory handle we cannot resolve). An operation on an
            //    under-root path whose other half we cannot even read is the
            //    last thing that should be forwarded blind.
            //
            // The rule this leaves is one sentence: a rename either has both
            // sides under the same root, and is routed, or it does not touch a
            // root at all, and is trampolined. Everything between is refused.
            if path_is_ours(&nt) {
                return STATUS_ACCESS_DENIED;
            }
        }
        // **A rename whose *target* lands under a managed root** (gate 5,
        // Task 5). Everything above is keyed on the *source*, and for a source
        // outside every root none of it runs: `record_path` inserts into
        // `PATH_TABLE` only when `path_is_ours(path)`, so an outside handle is
        // never recorded, the engine arm above is skipped, and `tramp` below
        // performed the move — physically creating a file under the
        // destination root, where it then read back as missing because that
        // root seals every path the provider graph does not serve.
        //
        // The destination is what decides containment. Content crossing *into*
        // the VFS by a route the director never saw is the same failure as
        // content crossing out of it, and the source being legitimately
        // outside does not make the target's root any less managed.
        //
        // Refused rather than routed, and there is no third option available:
        // `OP_RENAME` carries **one** root and two vpaths under it (see
        // `FuseClient::rename`), so the provider contract has no operation for
        // an import from outside. `STATUS_ACCESS_DENIED` rather than the
        // `STATUS_UNSUCCESSFUL` the cross-root arm above returns, because it is
        // a different answer: cross-root is "the graph cannot express this
        // move", this is "the destination will not accept content by this
        // route at all".
        //
        // NOTE: `parse_rename_target` discards `parent_dir_of_handle`'s
        // OS-consulted provenance bit, so a target named against a directory
        // handle the shim never saw opened reaches `path_is_ours` here without
        // an `UncachedScope`. That is the known gap `parse_rename_target`
        // already records for `engine.rename`, not a new one — it is listed
        // there rather than fixed here so both callers are fixed at once.
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
