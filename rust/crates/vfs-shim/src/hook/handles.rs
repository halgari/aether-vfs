//! Per-handle tracking tables: directory cursors, identities and paths of open handles.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{DirTracked, path_is_ours};
use std::collections::BTreeMap;
use std::sync::Mutex;
use vfs_redirect::nt_to_volume_relative;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// Handle value (`isize`) -> tracking. `BTreeMap::new()` is `const`, so this
/// needs no lazy init. Populated by the `NtCreateFile` hook, drained by
/// `NtClose`.
pub(super) static DIR_TABLE: Mutex<BTreeMap<isize, DirTracked>> = Mutex::new(BTreeMap::new());

/// Redirected-file handle -> virtual volume-relative path (identity spoof).
pub(super) static IDENTITY_TABLE: Mutex<BTreeMap<isize, String>> = Mutex::new(BTreeMap::new());

/// Any under-root open's handle -> the NT path it was opened as, so a later
/// handle-based delete/rename (NtSetInformationFile) can act by path.
pub(super) static PATH_TABLE: Mutex<BTreeMap<isize, String>> = Mutex::new(BTreeMap::new());

/// Record a freshly-opened handle as a candidate directory for enumeration
/// virtualization: only when the open succeeded and its path is under the
/// managed root. Harmless for file handles (they never receive a dir-enum call)
/// and reclaimed by `NtClose`. Shared by the `NtCreateFile` and `NtOpenFile`
/// pass-through paths.
///
/// Takes the already-decoded `path` rather than `oa`: `create_hook`/
/// `open_hook` decode once per invocation (`path_of_tracked`) and thread the
/// result through every function that used to call `path_of(oa)`
/// independently — including this one, `record_path`, `record_identity`,
/// `note_decision_outcome`, and `note_passthrough_outcome`. Before that, a
/// single hooked `NtCreateFile` could re-run the decode 2-5 times over; for
/// an unresolved handle-relative open that decode is `parent_dir_of_handle`'s
/// OS-consulted case 4 (a `GetFinalPathNameByHandleW` call), so re-running it
/// per caller meant several syscalls per open rather than one. Resolving once
/// and passing the `&str` down is safe to do — nothing changes underneath a
/// single hook invocation's decoded path in the window between its callers —
/// which is a different claim from *caching* it across invocations, and must
/// not be confused with the caching `vfs_redirect::UncachedScope` forbids for
/// an OS-consulted path (see `parent_dir_of_handle`'s case 4).
///
/// Caller's responsibility: hold a `vfs_redirect::UncachedScope` around this
/// call if `path` came from an OS-consulted decode — `path_is_ours` below
/// reaches the same cached `RootMap::under_root` `decision_for` does.
pub(super) unsafe fn tag_under_root(
    file_handle: *mut HANDLE,
    path: Option<&str>,
    status: NTSTATUS,
) {
    // NT_SUCCESS is status >= 0.
    if status < 0 || file_handle.is_null() {
        return;
    }
    let Some(path) = path else { return };
    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
    let key = unsafe { *file_handle } as isize;
    // Remember every handle's path, not just the ones under the root. NT lets a
    // caller open a file as (directory handle + leaf name), and without the
    // parent's path such an open cannot be decoded at all -- it is invisible to
    // every decision we make and reaches the real directory behind the mount.
    // The parent is often outside the root while the child is under it.
    //
    // Load-bearing for cost, not just correctness: `parent_dir_of_handle`'s
    // case 4 (OS-consulted fallback for a handle unseen by the shim) reasons
    // that it fires rarely *because* every handle the shim does see reach a
    // hooked create/open lands here unconditionally. Narrowing this insert to
    // only under-root handles (matching the `DIR_TABLE` insert just below)
    // would make case 4 fire for every outside-root ancestor handle too,
    // changing that branch from a rare safety net into a per-open cost.
    if let Ok(mut t) = HANDLE_PATHS.lock() {
        crate::breadcrumb::set_holder(crate::breadcrumb::holder::TAG_UNDER_ROOT);
        if t.len() < HANDLE_PATHS_MAX {
            t.insert(key, path.to_string());
        }
        crate::breadcrumb::set_holder(crate::breadcrumb::holder::NOBODY);
    }
    if path_is_ours(path) {
        if let Ok(mut table) = DIR_TABLE.lock() {
            table.insert(
                key,
                DirTracked {
                    dir_nt_path: path.to_string(),
                    state: None,
                },
            );
        }
    }
}

/// Handle -> the NT path it was opened as, for *every* successful open.
///
/// Reclaimed by `NtClose`; bounded so a handle leak cannot grow it without end.
pub(super) static HANDLE_PATHS: Mutex<BTreeMap<isize, String>> = Mutex::new(BTreeMap::new());
const HANDLE_PATHS_MAX: usize = 65_536;

pub(super) fn path_of_handle(handle: HANDLE) -> Option<String> {
    let g = HANDLE_PATHS.lock().ok()?;
    crate::breadcrumb::set_holder(crate::breadcrumb::holder::PATH_OF_HANDLE);
    let r = g.get(&(handle as isize)).cloned();
    crate::breadcrumb::set_holder(crate::breadcrumb::holder::NOBODY);
    r
}

/// Record a redirected handle's virtual identity: after a successful redirected
/// open, map the handle to the volume-relative form of the ORIGINAL virtual
/// path (the caller's `oa` still held it — only a local `new_oa` was
/// rewritten before the trampoline call). Reclaimed by `NtClose`. Enables the
/// `NtQueryInformationFile` class-48 spoof.
///
/// Takes the already-decoded `path` — see `tag_under_root`'s doc comment for
/// why callers thread this through rather than re-decoding independently.
pub(super) unsafe fn record_identity(
    file_handle: *mut HANDLE,
    path: Option<&str>,
    status: NTSTATUS,
) {
    if status < 0 || file_handle.is_null() {
        return;
    }
    if let Some(path) = path {
        if let Ok(mut t) = IDENTITY_TABLE.lock() {
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
            t.insert(
                unsafe { *file_handle } as isize,
                nt_to_volume_relative(path),
            );
        }
    }
}

/// Record a successful under-root open's handle -> folded vpath components, so
/// a later handle-based delete/rename can act by vpath. Shared by both open
/// hooks across all decision branches.
///
/// Takes the already-decoded `path` — see `tag_under_root`'s doc comment for
/// why callers thread this through rather than re-decoding independently.
/// Caller's responsibility: hold a `vfs_redirect::UncachedScope` around this
/// call if `path` came from an OS-consulted decode (`path_is_ours` below
/// reaches the same cached `RootMap::under_root` `decision_for` does).
pub(super) unsafe fn record_path(file_handle: *mut HANDLE, path: Option<&str>, status: NTSTATUS) {
    if status < 0 || file_handle.is_null() {
        return;
    }
    if let Some(path) = path {
        if path_is_ours(path) {
            if let Ok(mut t) = PATH_TABLE.lock() {
                // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
                t.insert(unsafe { *file_handle } as isize, path.to_string());
            }
        }
    }
}

/// Whether `handle` is a synthetic handle **that is currently open**.
///
/// `is_fuse_synth` alone is a bit-47 tag test, and that is not the same
/// question. `INVALID_HANDLE_VALUE` is `-1` — every bit set, including bit 47
/// — so it passes the tag test, and so does any synthetic handle that has
/// already been closed or was never issued. Answering `STATUS_SUCCESS` for
/// those turns a caller's error into a silent one: a lock on a handle whose
/// open actually failed would appear to be held.
///
/// Every other synthetic branch in this file resolves the handle before acting
/// (`read_hook`, `fuse_query_information`), which is why this exists rather
/// than the bare tag test the lock trio first shipped with.
pub(super) fn open_synth(handle: HANDLE) -> bool {
    crate::synth_file::is_fuse_synth(handle as isize)
        && crate::synth_file::lookup(handle as isize).is_some()
}

/// The NT path a synthetic handle was opened as, for the lock counters.
/// `None` for a handle no under-root open recorded.
pub(super) fn synth_path(handle: HANDLE) -> Option<String> {
    match PATH_TABLE.lock() {
        Ok(t) => t.get(&(handle as isize)).cloned(),
        Err(_) => None,
    }
}
