//! `NtClose`.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{HANDLES, TRAMP_CLOSE, reg_real};
use crate::ntdef::{STATUS_SUCCESS, STATUS_UNSUCCESSFUL};
use crate::sync::{CloseLock, lock_for_close};
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// Drop the handle's whole record from [`HANDLES`], non-blocking (`lock_for_close`).
fn forget_handle(handle: HANDLE) {
    if let Some(mut t) = lock_for_close(&HANDLES, &CloseLock::FILE) {
        crate::breadcrumb::set_holder(crate::breadcrumb::holder::CLOSE_HOOK);
        t.remove(handle as isize);
        crate::breadcrumb::set_holder(crate::breadcrumb::holder::NOBODY);
    }
}

/// Reclaim any tracking for a closing handle before the OS (possibly) reuses
/// its value.
pub(super) unsafe fn close_hook_body(handle: HANDLE) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Close);
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::ENTER);
    let tramp = match TRAMP_CLOSE.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::synth_file::is_fuse_synth(handle as isize) {
        crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_TABLE);
        if let Some((fh, delete)) = crate::synth_file::close_fuse(handle as isize) {
            crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_CLIENT);
            if let Some(c) = crate::director::global() {
                crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_RING);
                let _ = c.close(fh);
                // `FILE_DELETE_ON_CLOSE`: the delete happens now, at the director, as NT does it
                // at the last close. Wine's `DeleteFileW` is exactly this (an open with the flag,
                // then a close), so without it every Win32 delete of a served file was a silent
                // no-op under Proton. `NtClose` cannot report a failure (and `DeleteFileW` returns
                // TRUE either way), so a refusal is counted and named instead
                // (`hookstats::note_delete_on_close_refused`). It cannot be refused up front: the
                // open carries `DELETE` access, which is not a write open, and nothing in a read
                // open's answer says whether the path is deletable.
                if let Some(p) = delete.as_deref() {
                    if let Some((root, vp)) = c.route(p) {
                        c.names_changed(root, &vp);
                        crate::read_cache::invalidate_path(root.0, &vp);
                        if let Err(st) = c.delete(root, &vp) {
                            crate::hookstats::note_delete_on_close_refused(p, st);
                        }
                    }
                }
                crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_DONE);
            }
        }
        // The handle's record in `HANDLES` goes with it. Synthetic values only increase, so
        // nothing would ever reuse the key to clear it (`reset_key`): without this every
        // director-served open leaves a record for the life of the process, and after
        // `HANDLE_PATHS_MAX` of them no handle gets an `opened_as` at all.
        forget_handle(handle);
        crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_EXIT);
        return STATUS_SUCCESS;
    }
    if crate::synth_section::is_synth_section(handle as isize) {
        crate::breadcrumb::mark(crate::breadcrumb::mark_close::ZIP_TABLE);
        // Releasing shim-owned VA waits for the last view (NT semantics).
        if let Some(window) = crate::synth_section::close_section(handle as isize) {
            crate::breadcrumb::mark(crate::breadcrumb::mark_close::ZIP_REGION);
            crate::lazy_section::on_section_closed(window);
            crate::breadcrumb::mark(crate::breadcrumb::mark_close::ZIP_DONE);
        }
        crate::breadcrumb::mark(crate::breadcrumb::mark_close::ZIP_EXIT);
        return STATUS_SUCCESS;
    }
    // Registry key handles: a synthetic one is answered here, a pass-through one loses its
    // record and is closed for real below (then `after_real_close`).
    let reg_close = if crate::regclient::enabled() {
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        match unsafe { crate::regkeys::close(&reg_real(), handle as isize) } {
            crate::regkeys::Close::Done(st) => return st,
            crate::regkeys::Close::Real(rec) => Some(rec),
        }
    } else {
        None
    };
    // `try_lock`, never `lock`: this is best-effort reclamation, and a blocking
    // acquisition here can hang the process for good. A thread killed while holding a
    // `std::sync::Mutex` leaves it locked and not poisoned, and an exiting process
    // closes handles from the one thread left. Losing a reclamation is harmless: the
    // entry is keyed by a handle that is about to become invalid.
    // See `sync::lock_for_close` and docs/shim-invariants.md, "Close-path locking".
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TABLES);
    forget_handle(handle);
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TRAMP);
    // SAFETY: the original NT function, called with valid NT arguments.
    let r = unsafe { tramp(handle) };
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TRAMP_DONE);
    if let Some(rec) = reg_close {
        crate::regkeys::after_real_close(handle as isize, rec, r);
    }
    r
}
