//! `NtClose`.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{DIR_TABLE, HANDLE_PATHS, IDENTITY_TABLE, PATH_TABLE, TRAMP_CLOSE, reg_real};
use crate::ntdef::{STATUS_SUCCESS, STATUS_UNSUCCESSFUL};
use crate::sync::{CloseLock, lock_for_close};
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

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
        if let Some(fh) = crate::synth_file::close_fuse(handle as isize) {
            crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_CLIENT);
            if let Some(c) = crate::director::global() {
                crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_RING);
                let _ = c.close(fh);
                crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_DONE);
            }
        }
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
    if let Some(mut table) = lock_for_close(&DIR_TABLE, &CloseLock::FILE) {
        table.remove(&(handle as isize));
    }
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TABLE_HANDLE_PATHS);
    if let Some(mut t) = lock_for_close(&HANDLE_PATHS, &CloseLock::FILE) {
        crate::breadcrumb::set_holder(crate::breadcrumb::holder::CLOSE_HOOK);
        t.remove(&(handle as isize));
        crate::breadcrumb::set_holder(crate::breadcrumb::holder::NOBODY);
    }
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TABLE_IDENTITY);
    if let Some(mut t) = lock_for_close(&IDENTITY_TABLE, &CloseLock::FILE) {
        t.remove(&(handle as isize));
    }
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TABLE_PATH);
    if let Some(mut t) = lock_for_close(&PATH_TABLE, &CloseLock::FILE) {
        t.remove(&(handle as isize));
    }
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TRAMP);
    // SAFETY: the original NT function, called with valid NT arguments.
    let r = unsafe { tramp(handle) };
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TRAMP_DONE);
    if let Some(rec) = reg_close {
        crate::regkeys::after_real_close(handle as isize, rec, r);
    }
    r
}
