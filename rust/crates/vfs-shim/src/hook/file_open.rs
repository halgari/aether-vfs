//! `NtCreateFile` and `NtOpenFile`, and the director round trip that serves an under-root open.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{
    allow_disk_fallthrough, fuse_root_directory, in_hook_reenter, object_name_str, path_is_ours,
    path_of_tracked, record_path, reset_handle, reset_key, tag_under_root, to_nt_path, ShimIoGuard,
    HANDLES, TRAMP_CREATE, TRAMP_OPEN,
};
use crate::ntbuf::OwnedOa;
use crate::ntdef::{
    ObjectAttributes, FILE_CREATED, FILE_DELETE_ON_CLOSE, FILE_DIRECTORY_FILE,
    STATUS_ACCESS_DENIED, STATUS_FILE_IS_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SUCCESS,
    STATUS_UNSUCCESSFUL,
};
use core::ffi::c_void;
use std::sync::OnceLock;
use vfs_ntlayout::{
    dir_open_downgrades, disposition_information, disposition_needs_existence_probe,
    is_append_only, is_write_open, open_create_flags,
};
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// Record an open the director did not answer that passes through to the real filesystem,
/// unless `already` is set — meaning `try_fuse_create` already classified this same physical
/// open (the write fallback behind `allow_disk_fallthrough`) before returning `None`.
///
/// Scoped to opens **under a managed root**: a pass-through also fires for every path outside
/// every root (the ordinary case, e.g. `kernel32.dll`), and counting those would drown the
/// fall-through signal in background noise unrelated to any bypass. Under a root it is reached
/// only behind `allow_disk_fallthrough`.
///
/// Cheap when disabled: the caller passes an already-decoded `path` (see `tag_under_root`'s doc
/// comment for why re-decoding per caller is the thing to avoid), and `note_open_outcome` is a
/// no-op when stats are disabled.
///
/// Caller's responsibility: hold a `vfs_redirect::UncachedScope` around this call if `path` came
/// from an OS-consulted decode — `path_is_ours` is `RootMap`-backed and cached.
fn note_passthrough_outcome(path: Option<&str>, already: bool) {
    if already || !crate::hookstats::enabled() {
        return;
    }
    if let Some(p) = path {
        if path_is_ours(p) {
            crate::hookstats::note_open_outcome(
                crate::hookstats::OpenOutcome::FellThroughPassthrough,
                p,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn try_fuse_create(
    file_handle: *mut HANDLE,
    oa: *const ObjectAttributes,
    // Already-decoded path for this same invocation (`create_hook`/`open_hook`
    // call `path_of_tracked` once and pass its `.path` down) — see
    // `tag_under_root`'s doc comment for why this is threaded through rather
    // than re-decoded here via `path_of(oa)`.
    path: Option<&str>,
    iosb: *mut c_void,
    write: bool,
    disposition: u32,
    create_flags: u32,
    append_only: bool,
    // Set to `true` when this call already recorded an `OpenOutcome` for the
    // physical open before returning `None`. Exactly one site does that — the
    // write fallback behind `allow_disk_fallthrough`. The caller records a
    // pass-through for every `None`, and without this out-param would count
    // the same open twice (`note_passthrough_outcome`).
    outcome_recorded: &mut bool,
) -> Option<NTSTATUS> {
    let client = crate::director::global()?;
    let path = path?.to_string();
    let (root, vp) = client.route(&path)?;
    let vp = vp.as_str();

    // No basename exceptions: `steam_appid.txt`, `SkyrimSELauncher.exe`,
    // `steam_api{,64}.dll` and `SkyrimSE.exe` are served or sealed like any other path
    // under the root, so nothing under a managed root reaches the real disk by name.
    // See docs/shim-invariants.md, "Sealed root: opens".
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    drm_exe_trace(&path, unsafe { fuse_root_directory(oa) }, write);

    // Every under-root open, read and write, goes through the director, and every
    // answer it gives, failures included, is this function's answer: no decision below
    // returns `None` except behind the `allow_disk_fallthrough` opt-out. A poisoned
    // synth table (`open_fuse_at_ex` answering `None`) is not reachable — nothing inside
    // those critical sections can unwind, and `contain_panic` would not make it safe,
    // since the guard's drop has already poisoned the lock — and it fails closed
    // (`STATUS_UNSUCCESSFUL`, the director's handle given back) rather than returning
    // `None`, which would send an under-root path to the real call.
    // See docs/shim-invariants.md, "Sealed root: opens".
    // (Primary stack is expanded to 16 MiB by vfs-inject; open is a shallow ring op.)
    // Only the three "conditional" dispositions need to know whether the
    // path pre-existed to report the right `IoStatusBlock.Information` (see
    // `disposition_information`); the other three have one fixed outcome.
    // This is a separate ring round-trip ahead of the open, so it is skipped
    // whenever the answer is not needed. There is an inherent TOCTOU window
    // between this probe and the open below — another writer could create or
    // delete the path in between — but the race can only skew the reported
    // Information (kernel32's ERROR_ALREADY_EXISTS hint), never the actual
    // create/open outcome, which the director still decides atomically.
    let existed_before = write
        && disposition_needs_existence_probe(disposition)
        && matches!(client.getattr(root, vp), Ok(a) if a.found);

    // Shadowed so a directory downgrade below can correct the
    // `IoStatusBlock.Information` too: an existing directory opened through
    // `FILE_OPEN`/`FILE_OPEN_IF` was *opened*, never created or overwritten.
    let mut write = write;
    let mut opened = if write {
        // A write-open can create the name, and a name is stored as it is
        // created: so this one request carries the caller's spelling, not the
        // folded path every other request sends. See
        // `FuseClient::vpath_as_spelled`.
        let spelled = client.vpath_as_spelled(&path).map(|(_, v)| v);
        let created_as = match spelled.as_deref() {
            Some("") | None => vp,
            Some(v) => v,
        };
        let opened = client.open_write(root, created_as, create_flags);
        // Whatever was remembered about this name may no longer be so.
        client.names_changed(root, vp);
        opened
    } else {
        client.open(root, vp)
    };
    // A write-flavoured open of a directory is not a data write — see
    // `dir_open_downgrades`. Re-issued as a read open, which is what produces
    // the directory handle the caller actually asked for. Costs one extra
    // GETATTR, and only on a write open the director already refused.
    if write
        && opened.is_err()
        && dir_open_downgrades(disposition)
        && matches!(client.getattr(root, vp), Ok(a) if a.found && a.is_dir)
    {
        // Second `OP_OPEN` for one `Routed`: the caller records the outcome
        // once for this open, but the director's own arrived-open counter
        // sees two. Counted so the shim/director reconciliation stays an
        // exact equality — see `hookstats::UNROUTED_DIRECTOR_OPENS`.
        crate::hookstats::note_unrouted_director_open();
        opened = client.open(root, vp);
        write = false;
    }
    match opened {
        Ok(resp) => {
            // Record absolute path on the handle so later relative opens
            // (RootDirectory=this handle) resolve through the director.
            // A poisoned synth table (not reachable: see the doc at the top of this function)
            // fails the open closed and gives the director its handle back. Returning `None`
            // would send an under-root path on to the real call.
            let Some(h) = crate::synth_file::open_fuse_at_ex(
                resp.fh,
                resp.size,
                resp.is_dir,
                Some(path.clone()),
                append_only,
            ) else {
                let _ = client.close(resp.fh);
                return Some(STATUS_UNSUCCESSFUL);
            };
            // Every file handle joins the read cache's view of its file: a
            // write or mutable open drops what it holds of it; an immutable
            // read open may be served from it.
            if let Some(cache) = crate::read_cache::register(root.0, vp, &resp, write) {
                crate::synth_file::set_cache(h, cache);
            }
            if !file_handle.is_null() {
                // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
                unsafe {
                    *file_handle = h as HANDLE;
                }
            }
            let info = if write {
                disposition_information(disposition, existed_before)
            } else {
                crate::ntdef::FILE_OPENED
            };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, info) };
            // Direct PATH_TABLE insert with absolute path (path_of may be relative OA).
            reset_key(h, true);
            if let Ok(mut t) = HANDLES.lock() {
                t.set_under_root(h, path.clone());
            }
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { record_path(file_handle, Some(&path), STATUS_SUCCESS) };
            if resp.is_dir {
                // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                unsafe { tag_under_root(file_handle, Some(&path), STATUS_SUCCESS) };
            }
            director_open_trace(&path, resp.size);
            Some(STATUS_SUCCESS)
        }
        // Not in director: seal the path, for reads and writes alike. The only way out of
        // this arm without a status is the `VFS_ALLOW_DISK_FALLTHROUGH` opt-out, which
        // unseals the root wholesale (see `allow_disk_fallthrough`). A write used to fall
        // through to the (since removed) shim-local engine here; it no longer does.
        // See docs/shim-invariants.md, "Sealed root: statuses".
        Err(st) if st == vfs_protocol::ST_NOT_FOUND => {
            if allow_disk_fallthrough() {
                // The root is unsealed by operator opt-in. A write really does
                // fall through here, so it is still recorded as one — this is
                // the last site that can move `FellThroughWriteFallback` off
                // zero, and a live report showing it non-zero now means
                // exactly one thing: this switch is on. (Reads stay
                // unrecorded here: the caller records them as a pass-through.)
                if write {
                    crate::hookstats::note_open_outcome(
                        crate::hookstats::OpenOutcome::FellThroughWriteFallback,
                        &path,
                    );
                    *outcome_recorded = true;
                }
                None
            } else if write {
                // Two failures wear `ST_NOT_FOUND` on a write open, and NT distinguishes them:
                // - a create (`OPEN_CREATE`) that no writable provider can serve is
                //   `STATUS_OBJECT_PATH_NOT_FOUND`, as NTFS answers a create whose directory is
                //   missing;
                // - an open of an absent file is `STATUS_OBJECT_NAME_NOT_FOUND`, the same as the
                //   read seal below, so the open-for-write-then-create idiom still works.
                Some(if create_flags & vfs_protocol::OPEN_CREATE != 0 {
                    STATUS_OBJECT_PATH_NOT_FOUND
                } else {
                    STATUS_OBJECT_NAME_NOT_FOUND
                })
            } else {
                Some(STATUS_OBJECT_NAME_NOT_FOUND)
            }
        }
        // `OPEN_EXCL` (CREATE_NEW / FILE_CREATE) against a path that already
        // exists. Without this arm it fell into the generic `Err(_) if write`
        // guard below and fell through to the (since removed) shim-local overlay, which
        // *created the file there and reported success* — an exclusive
        // create silently "succeeding" against an existing file. Report the
        // real collision instead of falling through.
        Err(st) if st == vfs_protocol::ST_EXISTS => Some(STATUS_OBJECT_NAME_COLLISION),
        // Any other director error on a write, by cause:
        // - `ST_READ_ONLY` (no `ReadWrite` provider serves the path) is `STATUS_ACCESS_DENIED`;
        // - `ST_IS_DIR` (a file create aimed at a directory) is `STATUS_FILE_IS_A_DIRECTORY`;
        // - anything else is `STATUS_UNSUCCESSFUL`, like the read side.
        // No `allow_disk_fallthrough` escape: that relaxes "the director does not have
        // this", never "the director failed".
        // See docs/shim-invariants.md, "Sealed root: statuses".
        Err(st) if write => Some(match st {
            vfs_protocol::ST_READ_ONLY => STATUS_ACCESS_DENIED,
            vfs_protocol::ST_IS_DIR => STATUS_FILE_IS_A_DIRECTORY,
            _ => STATUS_UNSUCCESSFUL,
        }),
        Err(_) => {
            // Director down / I/O — do not fall through to the Steam tree.
            Some(STATUS_UNSUCCESSFUL)
        }
    }
}

/// Trace every under-root `SkyrimSE.exe` open. Off unless `VFS_DRM_EXE_LOG`
/// names a file.
///
/// `rel` marks a FUSE-relative OA (RootDirectory is a synthetic handle), which
/// is the shape the deleted exception blamed for `STATUS_OBJECT_NAME_NOT_FOUND`
/// — a shape that can no longer arise for this name, since the open is now
/// answered here rather than handed back to the kernel.
///
/// **Called on every under-root open**, so the enabled-check comes first and is
/// cached: the basename test is not free, and this is the hottest path in the
/// shim. Caching means a `VFS_DRM_EXE_LOG` set *after* the first open of the
/// process does not take effect; a diagnostic switch read once at startup is
/// the same contract every other switch in this file has.
///
/// The `route=` field the old format carried is gone: there is one route now.
fn drm_exe_trace(nt_or_win_path: &str, rel: bool, write: bool) {
    static LOG: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let Some(path) = LOG
        .get_or_init(|| vfs_env::path(vfs_env::DRM_EXE_LOG))
        .as_ref()
    else {
        return;
    };
    if !std::path::Path::new(nt_or_win_path)
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|b| b.eq_ignore_ascii_case("SkyrimSE.exe"))
    {
        return;
    }
    let p = crate::director::strip_nt_device(nt_or_win_path.trim()).replace('/', "\\");
    append_trace_line(
        path,
        &format!(
            "skyrimse-exe\toa={}\taccess={}",
            if rel { "fuse-relative" } else { "absolute" },
            if write { "write" } else { "read" },
        ),
        &p,
    );
}

/// Append `<epoch seconds>\t<head>\t<path>` to the diagnostic log at `log`. The shim's own file
/// I/O is wrapped in `ShimIoGuard`, so the append is not itself traced.
fn append_trace_line(log: &std::path::Path, head: &str, path: &str) {
    let line = format!(
        "{}\t{head}\t{path}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    let Some(_io) = ShimIoGuard::enter() else {
        return;
    };
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Optional proof that opens went through the director (not host disk).
/// Set `VFS_DIRECTOR_OPEN_LOG` to a file path.
fn director_open_trace(nt_or_win_path: &str, size: u64) {
    // Read once, like `drm_exe_trace`'s switch: this runs on every director open.
    static LOG: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    let Some(path) = LOG
        .get_or_init(|| vfs_env::path(vfs_env::DIRECTOR_OPEN_LOG))
        .as_ref()
    else {
        return;
    };
    let p = crate::director::strip_nt_device(nt_or_win_path.trim()).replace('/', "\\");
    let lower = p.to_ascii_lowercase();
    if !(lower.contains("\\data\\")
        || lower.ends_with(".esm")
        || lower.ends_with(".esl")
        || lower.ends_with(".esp")
        || lower.ends_with(".bsa")
        || lower.ends_with(".exe")
        || lower.ends_with(".dll")
        || lower.ends_with("steam_appid.txt"))
    {
        return;
    }
    append_trace_line(path, &format!("director-open\tsize={size}"), &p);
}

/// Create a virtual directory under the managed root via the ring (`OP_MKDIR`),
/// then hand back a virtual directory handle. Only acts on directory opens
/// (`FILE_DIRECTORY_FILE`) with a creating disposition (CREATE / OPEN_IF /
/// OVERWRITE_IF); a plain FILE_OPEN of an existing dir is left to
/// `try_fuse_create`. Returns `None` when it doesn't apply (not under root, not
/// a dir create, or no FUSE client) so the caller falls through.
unsafe fn try_fuse_mkdir(
    file_handle: *mut HANDLE,
    // Already-decoded path for this same invocation — see `tag_under_root`'s
    // doc comment for why callers thread this through rather than each
    // re-decoding via `path_of(oa)` independently.
    path: Option<&str>,
    iosb: *mut c_void,
    opts: u32,
    disp: u32,
) -> Option<NTSTATUS> {
    if opts & FILE_DIRECTORY_FILE == 0 {
        return None;
    }
    // FILE_CREATE(2), FILE_OPEN_IF(3), FILE_OVERWRITE_IF(5) create if absent.
    if !matches!(disp, 2 | 3 | 5) {
        return None;
    }
    let client = crate::director::global()?;
    let path = path?;
    // A directory is named as it is created: the caller's spelling, not the
    // folded path. See `FuseClient::vpath_as_spelled`.
    let (root, vp) = client.route_as_spelled(path)?;
    let vp = vp.as_str();
    client.names_changed(root, &vfs_core::fold(vp));
    match client.mkdir(root, vp, 0o755) {
        Ok(()) => {
            // Synthesize a virtual directory handle directly — do NOT OP_OPEN the
            // new dir: the overlay opens paths as FileChannels, and a directory
            // open throws. fh=0 is never a real director handle, so the NtClose-time
            // close(0) is a harmless no-op. The caller (CreateDirectoryW) only
            // needs a handle to receive and immediately close; later metadata
            // reads are path-based (qattr/getattr), not through this handle.
            let h = match crate::synth_file::open_fuse(0, 0, true) {
                Some(h) => h,
                // A poisoned synth table: fail closed rather than fall through to a real mkdir.
                None => return Some(STATUS_UNSUCCESSFUL),
            };
            if !file_handle.is_null() {
                // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
                unsafe {
                    *file_handle = h as HANDLE;
                }
            }
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, FILE_CREATED) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { reset_handle(file_handle, Some(path), STATUS_SUCCESS) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { record_path(file_handle, Some(path), STATUS_SUCCESS) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { tag_under_root(file_handle, Some(path), STATUS_SUCCESS) };
            Some(STATUS_SUCCESS)
        }
        // Parent missing → name-not-found (do not fall through to a real on-disk
        // mkdir under the root).
        Err(st) if st == vfs_protocol::ST_NOT_FOUND => Some(STATUS_OBJECT_NAME_NOT_FOUND),
        // mkdir failed — most often the directory already exists (the overlay
        // raises :already-exists, which the director has no dedicated status for and
        // reports as a generic error). Probe: if a directory is really there,
        // honor the disposition — FILE_CREATE(2) must report a name collision
        // (ERROR_ALREADY_EXISTS, so the create-and-ignore idiom works), while
        // FILE_OPEN_IF(3)/FILE_OVERWRITE_IF(5) open the existing directory.
        Err(_) => match client.getattr(root, vp) {
            Ok(a) if a.found && a.is_dir => {
                if disp == 2 {
                    Some(STATUS_OBJECT_NAME_COLLISION)
                } else {
                    let h = match crate::synth_file::open_fuse(0, 0, true) {
                        Some(h) => h,
                        // A poisoned synth table: fail closed rather than fall through to a real mkdir.
                        None => return Some(STATUS_UNSUCCESSFUL),
                    };
                    if !file_handle.is_null() {
                        // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
                        unsafe {
                            *file_handle = h as HANDLE;
                        }
                    }
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    unsafe {
                        crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, crate::ntdef::FILE_OPENED)
                    };
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    unsafe { reset_handle(file_handle, Some(path), STATUS_SUCCESS) };
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    unsafe { record_path(file_handle, Some(path), STATUS_SUCCESS) };
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    unsafe { tag_under_root(file_handle, Some(path), STATUS_SUCCESS) };
                    Some(STATUS_SUCCESS)
                }
            }
            _ => Some(STATUS_UNSUCCESSFUL),
        },
    }
}

/// Arity mirrors `NtCreateFile` exactly; it is not ours to reduce. (Clippy
/// exempts `extern` fns from this lint and this body is no longer one — the
/// `extern "system"` entry point is `create_hook`, generated by
/// `hook_entry_points!`.)
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn create_hook_body(
    file_handle: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    alloc: *const i64,
    attrs: u32,
    share: u32,
    disp: u32,
    opts: u32,
    ea: *const c_void,
    ealen: u32,
) -> NTSTATUS {
    let mut _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Create);
    let tramp = match TRAMP_CREATE.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // Re-entrant host probes (is_file / log append) must hit the real ntdll.
    if in_hook_reenter() {
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe {
            tramp(
                file_handle,
                access,
                oa,
                iosb,
                alloc,
                attrs,
                share,
                disp,
                opts,
                ea,
                ealen,
            )
        };
    }
    let call = OpenCall {
        file_handle,
        access,
        oa,
        iosb,
        disp,
        opts,
        create: true,
    };
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    unsafe {
        // The closure calls the original NT function with valid NT arguments.
        route_open(&call, &mut _hs, &|oa| {
            tramp(
                file_handle,
                access,
                oa,
                iosb,
                alloc,
                attrs,
                share,
                disp,
                opts,
                ea,
                ealen,
            )
        })
    }
}

/// An owned absolute copy of `oa` naming `nt`, with the caller's own `Length` echoed.
pub(super) unsafe fn redirected_oa(
    oa: *const ObjectAttributes,
    nt: &str,
) -> Result<Box<OwnedOa>, NTSTATUS> {
    // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
    unsafe { Ok(OwnedOa::absolute(Some(&*oa), nt, false)?.with_length((*oa).length)) }
}

/// The arguments of an `NtCreateFile` or `NtOpenFile` call that routing looks at.
pub(super) struct OpenCall {
    pub file_handle: *mut HANDLE,
    pub access: u32,
    pub oa: *const ObjectAttributes,
    pub iosb: *mut c_void,
    /// `NtOpenFile` has no disposition; it always opens existing, so its `disp` is `FILE_OPEN`
    /// (1), NOT 0: 0 is `FILE_SUPERSEDE`, which is in `is_write_open`'s create/overwrite set and
    /// would misclassify every open as a write.
    pub disp: u32,
    pub opts: u32,
    /// An `NtCreateFile`: a directory create under a managed root goes to the director's mkdir
    /// first, and a routed open counts whether the caller asked for synchronous I/O.
    pub create: bool,
}

/// The routing of an `NtCreateFile` or `NtOpenFile` call past the in-hook re-entry check: the
/// director's answer for an under-root open, else the real call (a path outside every root, or
/// an under-root miss behind `allow_disk_fallthrough`). `real` makes the original call with the
/// `OBJECT_ATTRIBUTES` it is given.
///
/// A name the call carries that cannot be decoded is refused (`path_of_tracked`'s status, the
/// C2 fail-closed rule), before anything else happens.
///
/// # Safety
/// The arguments are the caller's NT arguments.
unsafe fn route_open(
    call: &OpenCall,
    hs: &mut crate::hookstats::Timed,
    real: &dyn Fn(*const ObjectAttributes) -> NTSTATUS,
) -> NTSTATUS {
    let OpenCall {
        file_handle,
        access,
        oa,
        iosb,
        disp,
        opts,
        create,
    } = *call;
    // Decode once for the whole call and thread the result through every
    // function below that used to call `path_of(oa)` independently — see
    // `tag_under_root`'s doc comment for the cost argument.
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    let decoded = match unsafe { path_of_tracked(oa) } {
        Ok(d) => d,
        Err(st) => return st,
    };
    let path: Option<&str> = decoded.as_ref().map(|d| d.path.as_str());
    let os_consulted = decoded.as_ref().is_some_and(|d| d.os_consulted);
    // Held for the rest of this call whenever `path` is itself a snapshot of
    // a live OS query (an unseen handle's current target — `parent_dir_of_handle`
    // case 4) rather than a pure function of its own bytes: every
    // `RootMap`-backed decision made below with `path` (`FuseClient::route`, and
    // `path_is_ours` via `tag_under_root`/`record_path`/
    // `note_passthrough_outcome`) must not be cached under it. See
    // `vfs_redirect::UncachedScope`'s doc comment.
    let _uncached_guard = os_consulted.then(vfs_redirect::UncachedScope::enter);

    // Directory create under the managed root → ring OP_MKDIR (must precede the
    // generic file open below, which would otherwise create a FILE named as the
    // directory via the write-create path).
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    if create {
        if let Some(st) = unsafe { try_fuse_mkdir(file_handle, path, iosb, opts, disp) } {
            return st;
        }
    }
    // Prefer director FUSE for managed-root content (no in-shim synth_section).
    match path {
        Some(p) => crate::hookstats::note_passthrough(p),
        // An open we cannot decode is an open we cannot serve. If the masters
        // are hiding anywhere, it is here.
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        None => crate::hookstats::note_undecodable(unsafe { object_name_str(oa) }.as_deref()),
    }
    // Set by `try_fuse_create` when it already recorded an outcome (the write
    // fallback) for this open before returning `None` — see
    // `note_passthrough_outcome` for why that suppresses the recording below.
    let mut outcome_recorded = false;
    // `open_create_flags(FILE_OPEN)` is 0: an open-only call never creates,
    // truncates, or excludes.
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    if let Some(st) = unsafe {
        try_fuse_create(
            file_handle,
            oa,
            path,
            iosb,
            is_write_open(access, disp),
            disp,
            open_create_flags(disp),
            is_append_only(access),
            &mut outcome_recorded,
        )
    } {
        if crate::hookstats::enabled() {
            if let Some(p) = path {
                crate::hookstats::note_trace("open", p, if st >= 0 { "ok" } else { "FAIL" });
                crate::hookstats::note_open_outcome(crate::hookstats::OpenOutcome::Routed, p);
            }
        }
        hs.mark_rooted();
        // The director has no delete-on-close of its own: remember it on the synthetic handle,
        // and the close does the delete (`close_hook_body`).
        if st >= 0 && opts & FILE_DELETE_ON_CLOSE != 0 && !file_handle.is_null() {
            // SAFETY: raw access under the NT-pointer contract (hook/mod.rs); the open succeeded,
            // so the handle slot holds the synthetic handle it wrote.
            let h = unsafe { *file_handle } as isize;
            if crate::synth_file::is_fuse_synth(h) {
                crate::synth_file::set_delete_on_close(h, true);
            }
        }
        if create {
            // FILE_SYNCHRONOUS_IO_ALERT | FILE_SYNCHRONOUS_IO_NONALERT. Absent means
            // the caller intends asynchronous completion, which a synthetic handle
            // cannot deliver by APC or completion port.
            crate::hookstats::note_open_sync(opts & 0x0000_0030 != 0);
        }
        return st;
    }
    // The director did not answer: the path is outside every root, or under one behind
    // `allow_disk_fallthrough`. Either way the real filesystem answers.
    note_passthrough_outcome(path, outcome_recorded);
    // Never pass a FUSE RootDirectory to the kernel (invalid handle): rebuild an absolute OA
    // (null RootDirectory) from the decoded path instead. The case is narrow: a synthetic
    // `RootDirectory` whose recorded path joined with the relative name lands under no root
    // (a `..` climbing out of the root), so `try_fuse_create` declined it, and the synthetic
    // handle would fail at the kernel with a misleading status.
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    if unsafe { fuse_root_directory(oa) } {
        if let Some(path) = path {
            let nt = to_nt_path(path);
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            let status = match unsafe { redirected_oa(oa, &nt) } {
                Ok(new_oa) => real(new_oa.as_ptr()),
                Err(st) => st,
            };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { reset_handle(file_handle, Some(path), status) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { tag_under_root(file_handle, Some(path), status) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { record_path(file_handle, Some(path), status) };
            return status;
        }
        return STATUS_OBJECT_NAME_NOT_FOUND;
    }
    let status = real(oa);
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    unsafe { reset_handle(file_handle, path, status) };
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    unsafe { tag_under_root(file_handle, path, status) };
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    unsafe { record_path(file_handle, path, status) };
    status
}

/// `NtOpenFile` hook. Mirrors `create_hook` (director / pass-through + dir
/// tagging) for the open path that Rust `std` and many Win32 callers use to
/// open existing files and directories.
pub(super) unsafe fn open_hook_body(
    file_handle: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    share: u32,
    opts: u32,
) -> NTSTATUS {
    let mut _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Open);
    let tramp = match TRAMP_OPEN.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if in_hook_reenter() {
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(file_handle, access, oa, iosb, share, opts) };
    }
    let call = OpenCall {
        file_handle,
        access,
        oa,
        iosb,
        disp: vfs_ntlayout::FILE_OPEN,
        opts,
        create: false,
    };
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    unsafe {
        // The closure calls the original NT function with valid NT arguments.
        route_open(&call, &mut _hs, &|oa| {
            tramp(file_handle, access, oa, iosb, share, opts)
        })
    }
}
