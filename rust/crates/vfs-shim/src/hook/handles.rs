//! Per-handle tracking tables: directory cursors and paths of open handles.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{DirTracked, path_is_ours};
use std::collections::BTreeMap;
use std::sync::Mutex;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// What the shim knows about one open handle. Each field is set by its own kind of open and
/// all of them go when the handle is closed.
#[derive(Default)]
pub(super) struct HandleInfo {
    /// The NT path the handle was opened as, for *every* successful open (`tag_under_root`).
    /// Bounded by [`HANDLE_PATHS_MAX`] so a handle leak cannot grow it without end.
    pub(super) opened_as: Option<String>,
    /// An under-root open's NT path, so a later handle-based delete/rename (`NtSetInformationFile`)
    /// can act by path (`record_path`).
    pub(super) under_root: Option<String>,
    /// A candidate directory for enumeration virtualisation: an under-root open's cursor state.
    /// Harmless for file handles (they never receive a dir-enum call).
    pub(super) dir: Option<DirTracked>,
}

/// Handle value (`isize`) -> [`HandleInfo`], one table and one lock for everything the shim tracks
/// per handle. `NtClose` drops a handle's whole entry in one `try_lock`.
///
/// Fields are set one at a time and never cleared before the entry goes: a handle value the OS
/// reuses before its stale entry was reclaimed (a close that lost its `try_lock`) keeps whatever
/// the old handle's fields were, exactly as the tables this replaces (`DIR_TABLE`, `PATH_TABLE`,
/// `HANDLE_PATHS`) did one by one.
pub(super) struct HandleTable {
    map: BTreeMap<isize, HandleInfo>,
    /// Entries with `opened_as` set, which [`HANDLE_PATHS_MAX`] bounds.
    opened: usize,
}

impl HandleTable {
    const fn new() -> Self {
        HandleTable {
            map: BTreeMap::new(),
            opened: 0,
        }
    }

    /// Remember the path a handle was opened as, unless the bound is reached.
    pub(super) fn set_opened_as(&mut self, key: isize, path: String) {
        if self.opened >= HANDLE_PATHS_MAX {
            return;
        }
        let entry = self.map.entry(key).or_default();
        if entry.opened_as.replace(path).is_none() {
            self.opened += 1;
        }
    }

    pub(super) fn set_under_root(&mut self, key: isize, path: String) {
        self.map.entry(key).or_default().under_root = Some(path);
    }

    pub(super) fn set_dir(&mut self, key: isize, dir: DirTracked) {
        self.map.entry(key).or_default().dir = Some(dir);
    }

    pub(super) fn opened_as(&self, key: isize) -> Option<&String> {
        self.map.get(&key)?.opened_as.as_ref()
    }

    pub(super) fn under_root(&self, key: isize) -> Option<&String> {
        self.map.get(&key)?.under_root.as_ref()
    }

    pub(super) fn dir(&self, key: isize) -> Option<&DirTracked> {
        self.map.get(&key)?.dir.as_ref()
    }

    pub(super) fn dir_mut(&mut self, key: isize) -> Option<&mut DirTracked> {
        self.map.get_mut(&key)?.dir.as_mut()
    }

    /// Drop everything known about a handle.
    pub(super) fn remove(&mut self, key: isize) {
        if let Some(info) = self.map.remove(&key) {
            if info.opened_as.is_some() {
                self.opened -= 1;
            }
        }
    }
}

pub(super) static HANDLES: Mutex<HandleTable> = Mutex::new(HandleTable::new());

const HANDLE_PATHS_MAX: usize = 65_536;

/// Forget whatever the table holds for `key`, because a new handle with that value was just
/// issued. A handle value the OS (or the synthetic allocator) reuses can only have a stale
/// record, left behind by a close whose `try_lock` lost; without this, the stale `under_root`
/// path of a closed handle would be read as the new handle's (a `DeleteFile` of a real file
/// outside the root would then act on an unrelated path). The open then writes only what it
/// knows. `blocking` is for an open that goes on to take the table's lock anyway; otherwise
/// the clear is a `try_lock`, and a clear that loses it leaves the stale record, as before.
pub(super) fn reset_key(key: isize, blocking: bool) {
    if blocking {
        if let Ok(mut t) = HANDLES.lock() {
            t.remove(key);
        }
    } else if let Ok(mut t) = HANDLES.try_lock() {
        t.remove(key);
    }
}

/// [`reset_key`] for the handle a successful `NtCreateFile`/`NtOpenFile` returned. `path` is
/// the decoded path the open will record: with one, the open takes the lock anyway
/// (`tag_under_root`), so the clear blocks like it does; without one (nothing is recorded) it
/// is a `try_lock`, so a pass-through open that took no lock before still takes none that can
/// block. The residual window: a path-less open whose clear loses the `try_lock` keeps a stale
/// record, but nothing reads a record for it except by its own handle value.
pub(super) unsafe fn reset_handle(file_handle: *mut HANDLE, path: Option<&str>, status: NTSTATUS) {
    if status < 0 || file_handle.is_null() {
        return;
    }
    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
    reset_key(unsafe { *file_handle } as isize, path.is_some());
}

/// Record a freshly-opened handle as a candidate directory for enumeration
/// virtualization: only when the open succeeded and its path is under the
/// managed root. Harmless for file handles (they never receive a dir-enum call)
/// and reclaimed by `NtClose`. Shared by the `NtCreateFile` and `NtOpenFile`
/// pass-through paths.
///
/// Takes the already-decoded `path` rather than `oa`: `create_hook`/
/// `open_hook` decode once per invocation (`path_of_tracked`) and thread the
/// result through every function that used to call `path_of(oa)`
/// independently — including this one, `record_path` and `note_passthrough_outcome`. Before that, a
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
/// is `RootMap`-backed and cached.
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
    if let Ok(mut t) = HANDLES.lock() {
        crate::breadcrumb::set_holder(crate::breadcrumb::holder::TAG_UNDER_ROOT);
        t.set_opened_as(key, path.to_string());
        crate::breadcrumb::set_holder(crate::breadcrumb::holder::NOBODY);
    }
    if path_is_ours(path) {
        if let Ok(mut table) = HANDLES.lock() {
            table.set_dir(
                key,
                DirTracked {
                    dir_nt_path: path.to_string(),
                    state: None,
                },
            );
        }
    }
}

pub(super) fn path_of_handle(handle: HANDLE) -> Option<String> {
    let g = HANDLES.lock().ok()?;
    crate::breadcrumb::set_holder(crate::breadcrumb::holder::PATH_OF_HANDLE);
    let r = g.opened_as(handle as isize).cloned();
    crate::breadcrumb::set_holder(crate::breadcrumb::holder::NOBODY);
    r
}

/// The NT path an under-root open recorded for `handle` (`record_path`), if any.
pub(super) fn under_root_path(handle: isize) -> Option<String> {
    HANDLES.lock().ok()?.under_root(handle).cloned()
}

/// Record a successful under-root open's handle -> folded vpath components, so
/// a later handle-based delete/rename can act by vpath. Shared by both open
/// hooks across all decision branches.
///
/// Takes the already-decoded `path` — see `tag_under_root`'s doc comment for
/// why callers thread this through rather than re-decoding independently.
/// Caller's responsibility: hold a `vfs_redirect::UncachedScope` around this
/// call if `path` came from an OS-consulted decode (`path_is_ours` below
/// is `RootMap`-backed and cached).
pub(super) unsafe fn record_path(file_handle: *mut HANDLE, path: Option<&str>, status: NTSTATUS) {
    if status < 0 || file_handle.is_null() {
        return;
    }
    if let Some(path) = path {
        if path_is_ours(path) {
            if let Ok(mut t) = HANDLES.lock() {
                // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
                t.set_under_root(unsafe { *file_handle } as isize, path.to_string());
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
    under_root_path(handle as isize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fields_are_set_one_at_a_time_and_go_together() {
        let mut t = HandleTable::new();
        t.set_under_root(8, "under".into());
        assert_eq!(t.under_root(8).map(String::as_str), Some("under"));
        assert!(t.opened_as(8).is_none(), "setting one field sets no other");
        t.set_opened_as(8, "opened".into());
        t.set_dir(
            8,
            DirTracked {
                dir_nt_path: "dir".into(),
                state: None,
            },
        );
        assert_eq!(t.opened_as(8).map(String::as_str), Some("opened"));
        assert_eq!(t.dir(8).map(|d| d.dir_nt_path.as_str()), Some("dir"));
        assert!(t.under_root(9).is_none());
        t.remove(8);
        assert!(t.opened_as(8).is_none() && t.under_root(8).is_none());
        assert!(t.dir(8).is_none());
    }

    #[test]
    fn a_stale_record_is_cleared_when_a_new_open_reuses_the_handle_value() {
        // A distinct value: the global table is shared with other tests.
        let h: isize = 0x7ead_0001;
        {
            let mut t = HANDLES.lock().unwrap();
            // The record a close left behind when it lost its try_lock.
            t.set_under_root(h, "stale".into());
            t.set_opened_as(h, "stale".into());
            t.set_dir(
                h,
                DirTracked {
                    dir_nt_path: "stale".into(),
                    state: None,
                },
            );
        }
        let mut slot = h as HANDLE;
        // A failed open issued no handle: nothing is cleared.
        unsafe { reset_handle(&mut slot, Some("p"), -1) };
        assert!(HANDLES.lock().unwrap().under_root(h).is_some());
        // A successful one did (with and without a path to record).
        unsafe { reset_handle(&mut slot, Some("p"), 0) };
        {
            let t = HANDLES.lock().unwrap();
            assert!(t.under_root(h).is_none() && t.opened_as(h).is_none());
            assert!(t.dir(h).is_none());
        }
        HANDLES.lock().unwrap().set_under_root(h, "stale".into());
        unsafe { reset_handle(&mut slot, None, 0) };
        assert!(HANDLES.lock().unwrap().under_root(h).is_none());
    }

    #[test]
    fn the_opened_as_bound_counts_those_entries_only() {
        let mut t = HandleTable::new();
        for k in 0..HANDLE_PATHS_MAX as isize {
            t.set_opened_as(k, "p".into());
        }
        // Full: nothing more is recorded (an existing handle is not overwritten either, as
        // with `len() < MAX` on the table this replaced); the other fields are unaffected.
        t.set_opened_as(-1, "new".into());
        assert!(t.opened_as(-1).is_none());
        t.set_opened_as(5, "again".into());
        assert_eq!(t.opened_as(5).map(String::as_str), Some("p"));
        t.set_under_root(-2, "u".into());
        assert!(t.under_root(-2).is_some());
        // A removal makes room.
        t.remove(0);
        t.set_opened_as(-1, "new".into());
        assert_eq!(t.opened_as(-1).map(String::as_str), Some("new"));
    }
}
