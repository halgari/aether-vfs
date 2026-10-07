//! Directory enumeration: `NtQueryDirectoryFile` and `NtQueryDirectoryFileEx`.

use super::{DIR_TABLE, ENGINE, TRAMP_QDIR, TRAMP_QDIREX, path_of_handle};
use crate::ntdef::{
    SL_RESTART_SCAN, SL_RETURN_SINGLE_ENTRY, STATUS_BUFFER_OVERFLOW, STATUS_NO_MORE_FILES,
    STATUS_SUCCESS, STATUS_UNSUCCESSFUL, UnicodeString,
};
use core::ffi::c_void;
use vfs_redirect::{DirInfoClass, DirItem, DirStatus, write_dir_info};
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
    Ok(crate::ntbuf::us_string(file_name)?.filter(|s| !(s.is_empty() || s == "*" || s == "*.*")))
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
        let table = match DIR_TABLE.lock() {
            Ok(t) => t,
            Err(_) => return passthrough(),
        };
        match table.get(&key) {
            None => {
                drop(table);
                // Untracked: a directory outside the managed root, so the OS
                // answers. Worth recording anyway — "the game listed a Data
                // that isn't ours" is the diagnosis for an empty load order.
                if crate::hookstats::enabled() {
                    let dir = path_of_handle(handle).unwrap_or_else(|| "<unknown>".to_string());
                    crate::hookstats::note_readdir(
                        &dir,
                        wildcard_of(file_name).ok().flatten().as_deref(),
                        0,
                        crate::hookstats::ReadDirSource::Os,
                    );
                }
                return passthrough();
            }
            Some(t) => (restart || t.state.is_none(), t.dir_nt_path.clone()),
        }
    };

    // Phase 2 (unlocked): build the listing. The handle only reached
    // `DIR_TABLE` because `tag_under_root` found `path_is_ours` true for it,
    // so *every* listing built here is a listing under a managed root — and
    // the governing invariant says the real filesystem beneath a managed root
    // is unreachable by any spelling. A directory listing is a spelling. So
    // there are exactly two things that may appear in one:
    //
    // 1. What the director serves. When the FUSE client recognises the
    //    directory its `readdir` is the whole answer, authoritative and
    //    unmerged.
    // 2. Failing that, the shim-local write overlay's own entries — content
    //    this process created through gate 4's write path, which physically
    //    lives outside the root and which the director may not know about.
    //
    // What may **not** appear is the real directory behind the mount. Until
    // gate 4 task 8b this function had a third branch that drained exactly
    // that (`drain_real` over the handle) whenever the client was absent or
    // did not recognise the path, and put the overlay on top of it — so a
    // real, unserved file under a managed root would be listed. Reads,
    // metadata and writes were each sealed and proven by the escape matrix;
    // enumeration was only ever *argued* to follow from read-open containment,
    // and it does not follow: separate predicates, and no test on either side.
    //
    // **That drain was latent, not live** — say it here, not three paragraphs
    // down, because "task 8b closed a real-disk leak" read alone is the wrong
    // impression. `path_is_ours` is engine-OR-client while the client's
    // `RootMap` is the engine's roots plus the staging alias, so "engine
    // accepts, client declines" cannot arise; `RootMap::decide` denies
    // `NotFound`/`Dir`/`Tombstone` before any tramp call; neither
    // `Decision::Redirect` arm calls `tag_under_root`, so a redirected handle
    // never enters `DIR_TABLE`; and a director-served directory is a
    // `fuse_synth` handle the drain could not drain. Reaching the branch in a
    // test took reverting gate 3 task 5 *as well* as forcing the predicate
    // disagreement. The value of removing it is that enumeration no longer
    // depends, silently and untested, on another gate's invariant.
    //
    // `drain_real`, `drain_real_classic` and `parse_full_dir_info` are deleted
    // with it, so containment here is structural rather than conditional:
    // no code remains that can read a real directory into a served listing.
    //
    // The two ways of reaching case 2 answer the same way and are counted
    // separately, because they are different failures:
    //
    // - **No client at all.** Standalone mode is retired (see
    //   `fuse_client::FuseInitError`): bootstrap aborts the launch when the
    //   ring cannot be attached, and `try_init_from_env` runs before the
    //   engine is built and before any detour installs, so an injected process
    //   always has a client by the time a hook can fire.
    // - **A client that does not recognise this directory.** The engine's root
    //   notion accepted the path at open time and the client's did not — which
    //   the superset argument above says cannot happen, but these two
    //   predicates *have* drifted apart before, for five spellings at once,
    //   and the comment on `path_is_ours` says plainly that they "can differ".
    //   Its own counter (`contained`) so a future drift is a number in the
    //   report rather than a directory that mysteriously lists nothing.
    //
    // **Nothing reaches either one today, including this crate's own tests.**
    // An earlier draft claimed `hook_enum_parity`/`hook_relative_paths` did,
    // since they install with no ring; they do not. Their `Data` is
    // overlay-backed, so `Engine::decide` answers `Redirect`, which never
    // tags the handle — those listings leave on the untracked branch above,
    // against the overlay's own physical path. Measured with a probe in each
    // branch, not argued: zero hits on both, in all three shim enumeration
    // tests. So this arm and `Engine::overlay_listing`'s only call site are
    // dead code. Keep both anyway: a branch that would otherwise fail *open*
    // is exactly the one worth having fail closed, and the day it comes back
    // to life is the day someone changes a predicate.
    //
    // One consequence worth stating, because a reviewer read the other way
    // round: this arm calls `overlay_listing` with an **empty base**, so
    // `Overlay::apply_to_listing`'s handling of a `merged` listing is
    // unreachable from production even if this arm revives with today's call
    // shape.
    //
    // **Gate 5, Task 7 changed what that costs.** It used to mean the only
    // implementation of marker-hiding sat behind two dead callers while the
    // live director branch below went without. The filtering now lives in
    // `overlay::strip_whiteout_markers`, which that branch calls directly and
    // `apply_to_listing` also calls — so the dead pair is kept for the
    // fail-closed reason above and no longer holds a second, divergent copy
    // of anything that matters. What is left dead in `apply_to_listing` is
    // its *physical* overlay-directory scan, which answers a case the live
    // branch does not have (a marker on disk that the incoming listing does
    // not carry).
    //
    // The ring round trip and the overlay's own `read_dir` both call out, so
    // the lock must NOT be held here (NtClose also takes it).
    let rebuilt = if need_build {
        // A wildcard NT's own capture refuses gets NT's answer.
        let wildcard = match wildcard_of(file_name) {
            Ok(w) => w,
            Err(st) => return st,
        };
        let routed =
            crate::fuse_client::global().and_then(|c| c.route(&dir_path).map(|hit| (c, hit)));
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
                        // **Gate 5, Task 7 — the phantom whiteout marker,
                        // closed.** This branch used to hand the director's
                        // answer to the game verbatim, and the director's
                        // answer carries the shim's own markers: it mounts the
                        // shim overlay directory as its write layer
                        // (`overlay_layer_dir`) and spells whiteouts
                        // `.wh.<name>`, not `<name>.__vfs_wh__`, so ours come
                        // back as ordinary files. That showed the game a
                        // phantom `<file>.__vfs_wh__` entry *and* left the
                        // file it names listed.
                        //
                        // **Before the wildcard filter, not after** — see
                        // `strip_whiteout_markers`, which also records why the
                        // fix is here rather than in a shared spelling.
                        let mut items = crate::overlay::strip_whiteout_markers(items);
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
                // Two things this does **not** fix, both re-derived for this
                // task rather than inherited from the note that used to sit
                // here (which blamed a route gate 5 Task 4 had already
                // deleted):
                //
                // 1. **Enumeration only.** A marker still does not hide its
                //    target from an `open` through the director:
                //    `OverlayProvider::hidden_by_whiteout` looks for its own
                //    `.wh.` spelling, and there is no per-open hook here that
                //    could ask without a `stat` on every read.
                // 2. **New markers can still be minted under a live
                //    director.** `delete_hook` asks the client before
                //    `Engine::whiteout`, so a path-based delete routes; but
                //    `setinfo_hook`'s engine branch asks the engine *only*, so
                //    a handle-based delete on a non-synthetic under-root
                //    handle (inherited, pre-injection, or
                //    `allow_disk_fallthrough`) writes a shim-spelled marker
                //    into the director's own upper without the director ever
                //    hearing about the delete. That is a divergence between
                //    the two delete routes, not a listing defect, and it is
                //    recorded in gate 5's Task 7/8 report rather than changed
                //    at the end of a gate.
                Some((items, crate::hookstats::ReadDirSource::Director))
            }
            None => {
                // No real base to layer onto — that is the whole point. An
                // overlay-only listing is `overlay_listing` over an empty
                // base, which also means every entry now passes through the
                // wildcard filter: `apply_to_listing` only filters what it
                // *adds*, so the drained base used to skip the filter
                // entirely and answer `*.esp` with the whole directory.
                let items = match ENGINE.get() {
                    Some(engine) => engine.overlay_listing(&dir_path, &[], wildcard.as_deref()),
                    None => Vec::new(),
                };
                Some((items, crate::hookstats::ReadDirSource::ContainedNoDirector))
            }
        }
    } else {
        None
    };

    // Phase 3 (locked): store the built listing (if rebuilt) and serve a slice.
    //
    // **The caller's buffer is filled after the guard is released, never under
    // it.** `write_dir_info` writes into a scratch buffer we own; the copy into
    // `info` happens below, unlocked.
    //
    // That ordering is the fix for the intermittent hang traced on 2026-09-02.
    // `info` belongs to the caller and may lie inside one of our own
    // demand-paged regions, so touching it can fault into `lazy_section`, which
    // does file I/O, whose `NtClose` re-enters the shim and takes
    // `DIR_TABLE.lock()` again. `std::sync::Mutex` is not reentrant, so that
    // second acquisition blocked forever on a lock the same thread already
    // held: zero CPU, one thread, and immune to `TerminateProcess`. Measured
    // three times with `VFS_SHIM_BREADCRUMB` — `threads=1`, `entries - exits =
    // 2`, `mark=TABLES`.
    //
    // A scratch buffer rather than cloning the entries: the copy is bounded by
    // `length`, whereas a directory listing is unbounded.
    let mut scratch = vec![0u8; length as usize];
    let result = {
        let mut table = match DIR_TABLE.lock() {
            Ok(t) => t,
            Err(_) => return passthrough(),
        };
        let tracked = match table.get_mut(&key) {
            Some(t) => t,
            None => return passthrough(),
        };
        if let Some((entries, source)) = rebuilt {
            crate::hookstats::note_readdir(
                &dir_path,
                wildcard_of(file_name).ok().flatten().as_deref(),
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
        let buf = core::slice::from_raw_parts_mut(info as *mut u8, length as usize);
        buf[..result.bytes].copy_from_slice(&scratch[..result.bytes]);
    }

    let status = match result.status {
        DirStatus::Success => STATUS_SUCCESS,
        DirStatus::NoMoreFiles => STATUS_NO_MORE_FILES,
        DirStatus::BufferOverflow => STATUS_BUFFER_OVERFLOW,
    };
    // IO_STATUS_BLOCK: Status (NTSTATUS) @0, Information (ULONG_PTR) @8.
    crate::ntbuf::iosb_set(iosb, status, result.bytes);
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
