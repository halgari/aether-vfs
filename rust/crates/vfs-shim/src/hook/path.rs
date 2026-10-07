//! Decoding NT paths: `OBJECT_ATTRIBUTES` to a path, relative opens, rename targets.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{path_of_handle, under_root_path};
use crate::ntdef::ObjectAttributes;
use core::ffi::c_void;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// Decode ObjectName as UTF-16 (no root resolution). `None` for a NULL `oa` or `ObjectName`,
/// and for a name `ntbuf::us_units` rejects (odd length, NULL buffer with a length). Only the
/// hookstats undecodable-name counters use it; routing decisions go through `path_of`, and the
/// real syscall refuses such a name itself.
pub(super) unsafe fn object_name_str(oa: *const ObjectAttributes) -> Option<String> {
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    unsafe { crate::ntbuf::oa_name_string(oa) }.ok().flatten()
}

/// The process's current-directory handle and its DOS path, read from the PEB.
///
/// `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory` is the only place the handle
/// is published; there is no API that hands it back.
pub(super) unsafe fn cwd_from_peb() -> Option<(isize, String)> {
    // x64: TEB.ProcessEnvironmentBlock @ 0x60, PEB.ProcessParameters @ 0x20,
    // params.CurrentDirectory @ 0x38 = { UNICODE_STRING DosPath; HANDLE Handle }.
    let teb: usize;
    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
    unsafe {
        core::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, preserves_flags))
    };
    if teb == 0 {
        return None;
    }
    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
    let peb = unsafe { *((teb + 0x60) as *const usize) };
    if peb == 0 {
        return None;
    }
    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
    let params = unsafe { *((peb + 0x20) as *const usize) };
    if params == 0 {
        return None;
    }
    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
    let units = unsafe { *((params + 0x38) as *const u16) } as usize / 2;
    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
    let buf = unsafe { *((params + 0x40) as *const *const u16) };
    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
    let handle = unsafe { *((params + 0x48) as *const isize) };
    if buf.is_null() || units == 0 || handle == 0 {
        return None;
    }
    Some((
        handle,
        // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
        String::from_utf16_lossy(unsafe { core::slice::from_raw_parts(buf, units) }),
    ))
}

/// The directory that a relative name is expressed against, plus whether
/// finding it required consulting the OS about a handle's *current* target
/// rather than reading something the shim already knew deterministically.
/// See [`DecodedPath`] for why that distinction has to travel with the path.
///
/// Four kinds of parent reach us, and missing any one makes the child
/// undecodable — which is silent rather than an error: the call simply bypasses
/// every decision we would have made and lands on whatever is really on disk.
/// Shared by every hook that has to decode a name, so they cannot drift apart.
unsafe fn parent_dir_of_handle(root_handle: HANDLE) -> Option<(String, bool)> {
    let root = root_handle as isize;
    // 1. Our own synthetic directory handles.
    if crate::synth_file::is_fuse_synth(root) {
        // Prefer PATH_TABLE (recorded on open); fall back to synth_file abs_path.
        let p = under_root_path(root).or_else(|| crate::synth_file::abs_path(root))?;
        return Some((p, false));
    }
    // 2. A real directory the process opened; we remember every one.
    if let Some(p) = path_of_handle(root_handle) {
        return Some((p, false));
    }
    // 3. The current-directory handle. The OS creates it, so it is in no table
    //    of ours, yet it is the parent for every relative open a CRT makes:
    //    `CreateFileW("Data\X")` becomes (CWD handle + "Data\X").
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    if let Some((cwd_handle, dos)) = unsafe { cwd_from_peb() } {
        if cwd_handle == root {
            return Some((format!(r"\??\{}", dos.trim_end_matches(['\\', '/'])), false));
        }
    }
    // 4. A handle we never saw opened — opened before injection, inherited
    //    across a `CreateProcess`, or duplicated in from another process —
    //    so it appears in none of our tables and is not the PEB's CWD
    //    handle either. This is `NtCreateFile`'s
    //    `OBJECT_ATTRIBUTES.RootDirectory` vector: the game holds a real
    //    directory handle and names the child only relative to it, so the
    //    string a hook sees (`Skyrim\Data\a.esp`) cannot be related to the
    //    managed root by any amount of string canonicalisation — the root
    //    information lives in the handle, not the string. Ask the OS
    //    directly: `GetFinalPathNameByHandleW` on the handle itself needs no
    //    reopen, since we already hold it.
    //
    //    Its answer is `VOLUME_NAME_DOS` (`\\?\`-prefixed), not the `\??\`
    //    spelling a real NT open presents, but that is not parsed here —
    //    `path_of`'s callers always re-canonicalise the assembled path
    //    (`FuseClient::route` and `path_is_ours`, both `RootMap::resolve`),
    //    and `canonicalise` already treats `\\?\` as a
    //    recognised prefix. Handing back the OS string unparsed is exactly
    //    what `vfs_redirect::expand_short_name`'s callers already do with
    //    this same result shape (see its doc comment) — hand-parsing it here
    //    instead would be a second, drifting implementation of that same
    //    normalisation.
    //
    //    Expected to fire rarely: every handle the shim itself sees opened
    //    (case 2, above) is already free to answer from that table, whether
    //    or not it lies under the root — this branch is reached only for a
    //    handle the shim was not present to observe. Its cost is coupled to
    //    `tag_under_root` recording *every* handle unconditionally (not just
    //    ones under the root) — see that function's own doc comment.
    //
    //    SAFETY: `root_handle` is `OBJECT_ATTRIBUTES.RootDirectory` from an
    //    in-flight NT call this process is making right now — by
    //    construction a currently-valid, open handle owned by the caller
    //    (the game), which is exactly what `final_path_for_handle` requires.
    //    This function does not close it or otherwise take ownership of it.
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    let resolved = unsafe { vfs_win::final_path_for_handle(root_handle) }?;
    // `true`: this string is a snapshot of the handle's target *right now*,
    // not a pure function of anything in `root_handle`/the relative name
    // bytes — every caller that turns this into a `RootMap`-backed decision
    // must say so too (`vfs_redirect::UncachedScope`), or the decision could
    // be cached under a string that stops being true later in the session.
    Some((resolved, true))
}

/// A path decoded from an `OBJECT_ATTRIBUTES`, tagged with whether decoding it
/// required consulting the OS about a handle's current target
/// (`parent_dir_of_handle`'s case 4) rather than being derivable purely from
/// the shim's own tables, the raw name string, or the process's own PEB.
///
/// `os_consulted` is the caller-side half of `vfs_redirect::UncachedScope`'s
/// contract: any `RootMap`-backed decision made with `path` — `FuseClient::route`,
/// or `path_is_ours` — must hold that guard
/// for as long as it is deciding with `path`, because the answer is itself
/// only a fact about a handle's target *at this moment*, not a pure function
/// of `path`'s bytes that would be safe to cache under them.
pub(super) struct DecodedPath {
    pub(super) path: String,
    pub(super) os_consulted: bool,
}

/// Decode `oa` to a full path, once, tracking whether the decode needed an OS
/// consult. [`path_of`] is the provenance-blind convenience wrapper for the
/// many callers (filename matching, tracing, hookstats) that only ever read
/// the string and never feed it back into a cached `RootMap` decision.
///
/// `Err` is a name NT itself refuses (`ntbuf::us_units`: odd length, NULL buffer with a length).
/// The hook must return that status and not call the real syscall: the host would rebuild the
/// name (Wine rounds an odd length down) and act on a real file the shim would have virtualised.
/// `Ok(None)` is a name that is simply not decodable to a path.
pub(super) unsafe fn path_of_tracked(
    oa: *const ObjectAttributes,
) -> Result<Option<DecodedPath>, NTSTATUS> {
    if oa.is_null() {
        return Ok(None);
    }
    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
    let oa_ref = unsafe { &*oa };
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    let Some(name) = unsafe { crate::ntbuf::oa_name_string(oa) }? else {
        return Ok(None);
    };
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    Ok(unsafe { decode_relative(oa_ref, name) })
}

/// The path for an already-decoded `name` and the OA's `RootDirectory`.
unsafe fn decode_relative(oa_ref: &ObjectAttributes, name: String) -> Option<DecodedPath> {
    if oa_ref.root_directory.is_null() {
        return if name.is_empty() {
            None
        } else {
            Some(DecodedPath {
                path: name,
                os_consulted: false,
            })
        };
    }
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    let (parent, os_consulted) = unsafe { parent_dir_of_handle(oa_ref.root_directory) }?;
    let parent = parent.trim_end_matches(['\\', '/']);
    let rel = name.trim_start_matches(['\\', '/']);
    let path = if rel.is_empty() {
        parent.to_string()
    } else {
        format!("{parent}\\{rel}")
    };
    Some(DecodedPath { path, os_consulted })
}

/// Fully-qualified NT/Win32 path for an open.
///
/// Absolute names work as before. **Relative** opens (`RootDirectory` set) only
/// resolve when the root is a FUSE synthetic directory handle whose absolute
/// path was recorded in `PATH_TABLE`. Real kernel roots return `None` so the
/// caller can tramp. Without this, steam_api / CRT opens like
/// `RootDirectory=<game dir FUSE handle>, Name=steam_appid.txt` hit the kernel
/// with a fake handle → fail → **Steam Error**.
pub(super) unsafe fn path_of(oa: *const ObjectAttributes) -> Result<Option<String>, NTSTATUS> {
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    Ok(unsafe { path_of_tracked(oa) }?.map(|d| d.path))
}

/// Is this path one we are responsible for: under a root the director's client declared?
///
/// The same question `FuseClient::route` answers, asked without building the wire vpath. Every
/// caller must ask through here rather than test a root list of its own: when `tag_under_root`
/// asked a narrower question, the enumeration of `<stage>\Data` (the staging alias of root 0)
/// went untracked and fell through to the real staging folder, which returned nothing — and an
/// empty `Data` listing is an empty load order.
///
/// With no client attached (only a test that installs the detours without a ring) nothing is
/// ours.
pub(super) fn path_is_ours(path: &str) -> bool {
    crate::director::global().is_some_and(|c| c.vpath_under_root(path).is_some())
}

/// Parse the target path from a `FILE_RENAME_INFORMATION`(`_EX`) buffer. Only
/// absolute targets (RootDirectory == NULL) are handled; otherwise `None`.
pub(super) unsafe fn parse_rename_target(info: *mut c_void, length: u32) -> Option<String> {
    // SAFETY: `info` is NULL or valid for `length` bytes (hook/mod.rs).
    let buf = unsafe { super::caller_buf(info, length as usize) };
    let vfs_ntlayout::RenameTarget { root_dir, name } = vfs_ntlayout::parse_rename_info(buf)?;
    if root_dir == 0 {
        return Some(name);
    }
    // A target named against a directory handle. Callers feed this straight to
    // `vpath_under_root`, which needs a full path, so join it here — and
    // decline when the parent is unknown rather than passing a bare leaf name
    // off as if it were absolute.
    //
    // NOTE: discards `parent_dir_of_handle`'s OS-consulted provenance bit.
    // Its case-4 fallback can fire here exactly as it can for a handle-relative
    // create/open, and this rename path's callers (`FuseClient::route_as_spelled`,
    // `path_is_ours`) are `RootMap`-backed and cached — so an OS-consulted rename
    // target has the same caching exposure `create_hook`/`open_hook` were fixed
    // for, not yet closed here. Tracked as a known gap rather than silently
    // assumed safe.
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    let (parent, _os_consulted) = unsafe { parent_dir_of_handle(root_dir as HANDLE) }?;
    let parent = parent.trim_end_matches(['\\', '/']);
    let rel = name.trim_start_matches(['\\', '/']);
    if rel.is_empty() {
        return Some(parent.to_string());
    }
    Some(format!("{parent}\\{rel}"))
}

/// True when OA.RootDirectory is a FUSE synthetic handle.
pub(super) unsafe fn fuse_root_directory(oa: *const ObjectAttributes) -> bool {
    if oa.is_null() {
        return false;
    }
    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
    let root = unsafe { (*oa).root_directory };
    !root.is_null() && crate::synth_file::is_fuse_synth(root as isize)
}

/// Absolute `\??\` NT path for a Win32 or NT path string: `vfs_redirect::to_nt` after trimming
/// and normalising a `\\?\` long prefix to `\??\`.
pub(super) fn to_nt_path(path: &str) -> String {
    vfs_redirect::to_nt(crate::director::strip_nt_device(path.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook::HANDLES;
    use crate::hook::test_support::{oa_named, us_raw};
    use crate::ntdef::STATUS_OBJECT_NAME_INVALID;

    /// An `ObjectName` the file hooks cannot decode is left to the real syscall: an odd `Length`
    /// used to be read with its last byte dropped (so `"ab"` for 5 bytes of `"abc"`), and is
    /// now undecodable. A zero-length name with a NULL buffer is the empty string like any other
    /// zero-length name; it used to be undecodable.
    #[test]
    fn object_name_str_follows_the_shared_unicode_string_rule() {
        let mut w: Vec<u16> = "abc".encode_utf16().collect();
        let even = us_raw(6, w.as_mut_ptr());
        let odd = us_raw(5, w.as_mut_ptr());
        let null_empty = us_raw(0, core::ptr::null_mut());
        let null_len = us_raw(2, core::ptr::null_mut());
        unsafe {
            assert_eq!(object_name_str(&oa_named(&even)).as_deref(), Some("abc"));
            assert_eq!(object_name_str(&oa_named(&odd)), None);
            assert_eq!(object_name_str(&oa_named(&null_empty)).as_deref(), Some(""));
            assert_eq!(object_name_str(&oa_named(&null_len)), None);
            assert_eq!(object_name_str(core::ptr::null()), None);
        }
    }

    /// The decoders behind every file hook return NT's status for a name it refuses, and the
    /// hooks answer with it (`tests/seal/odd_length_name_sealed.rs` checks that end to end).
    #[test]
    fn path_of_tracked_reports_a_name_nt_refuses() {
        let mut w: Vec<u16> = "C:\\a".encode_utf16().collect();
        let even = us_raw(8, w.as_mut_ptr());
        let odd = us_raw(7, w.as_mut_ptr());
        let null_len = us_raw(2, core::ptr::null_mut());
        unsafe {
            assert_eq!(
                path_of_tracked(&oa_named(&even)).map(|d| d.map(|d| d.path)),
                Ok(Some("C:\\a".to_string()))
            );
            assert_eq!(
                path_of_tracked(&oa_named(&odd)).map(|d| d.map(|d| d.path)),
                Err(STATUS_OBJECT_NAME_INVALID)
            );
            assert_eq!(
                path_of_tracked(&oa_named(&null_len)).map(|d| d.map(|d| d.path)),
                Err(crate::ntdef::STATUS_ACCESS_VIOLATION)
            );
            assert!(path_of_tracked(core::ptr::null()).unwrap().is_none());
        }
    }

    /// `to_nt_path` is `vfs_redirect::to_nt` plus a trim and a long-prefix strip; every spelling
    /// of one path lands on the same `\??\` name.
    #[test]
    fn to_nt_path_gives_one_spelling() {
        for p in [r"C:\a\b", r"\??\C:\a\b", r"\\?\C:\a\b", "  C:\\a\\b "] {
            assert_eq!(to_nt_path(p), r"\??\C:\a\b", "{p:?}");
        }
    }

    /// FILE_RENAME_INFORMATION: ReplaceIfExists(1)+pad, RootDirectory@8,
    /// FileNameLength@16, FileName@20.
    fn rename_info(root_dir: usize, name: &str) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let namelen = units.len() * 2;
        let mut buf = vec![0u8; 20 + namelen];
        buf[8..16].copy_from_slice(&root_dir.to_le_bytes());
        buf[16..20].copy_from_slice(&(namelen as u32).to_le_bytes());
        for (i, u) in units.iter().enumerate() {
            buf[20 + i * 2..22 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
        buf
    }

    fn parse_rename(buf: &mut [u8]) -> Option<String> {
        unsafe { parse_rename_target(buf.as_mut_ptr() as *mut c_void, buf.len() as u32) }
    }

    #[test]
    fn an_absolute_rename_target_is_returned_as_is() {
        let mut buf = rename_info(0, r"\??\C:\root\new.esp");
        assert_eq!(
            parse_rename(&mut buf).as_deref(),
            Some(r"\??\C:\root\new.esp")
        );
    }

    /// A rename target may be named against a directory handle. Refusing to
    /// decode those is the same defect that made relative *opens* invisible —
    /// the rename would fall through unvirtualised and hit the real directory.
    #[test]
    fn a_rename_target_relative_to_a_known_handle_becomes_a_full_path() {
        let handle = 0x4321usize;
        HANDLES
            .lock()
            .unwrap()
            .set_opened_as(handle as isize, r"\??\C:\root\Data".to_string());

        let mut buf = rename_info(handle, "new.esp");
        assert_eq!(
            parse_rename(&mut buf).as_deref(),
            Some(r"\??\C:\root\Data\new.esp"),
            "a handle-relative target must be joined to its parent"
        );

        HANDLES.lock().unwrap().remove(handle as isize);
    }

    /// An unknown parent must yield nothing. Returning the bare leaf would be
    /// worse than declining: callers treat the result as a full path.
    #[test]
    fn a_rename_target_with_an_unknown_parent_is_declined() {
        let mut buf = rename_info(0xDEAD_BEEF, "new.esp");
        assert_eq!(parse_rename(&mut buf), None);
    }
}
