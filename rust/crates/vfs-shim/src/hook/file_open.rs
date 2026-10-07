//! `NtCreateFile` and `NtOpenFile`, and the director round trip that serves an under-root open.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{
    PATH_TABLE, ShimIoGuard, TRAMP_CREATE, TRAMP_OPEN, allow_disk_fallthrough, decision_for,
    fuse_root_directory, in_hook_reenter, object_name_str, path_is_ours, path_of_tracked,
    record_identity, record_path, tag_under_root, to_nt_path,
};
use crate::ntbuf::OwnedOa;
use crate::ntdef::{
    FILE_CREATED, FILE_DIRECTORY_FILE, NtCreateFileFn, NtOpenFileFn, ObjectAttributes,
    STATUS_ACCESS_DENIED, STATUS_FILE_IS_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SUCCESS,
    STATUS_UNSUCCESSFUL,
};
use core::ffi::c_void;
use std::sync::OnceLock;
use vfs_redirect::Decision;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

/// Record a `decision_for` fallthrough outcome (`Redirect`/`Deny`),
/// unless `already` is set — meaning `try_fuse_create` already classified this
/// same physical open (the write fallback, and only that: the DRM exception
/// was the other classifier here until gate 5 Task 4 deleted it) before
/// returning `None`. Both `create_hook` and `open_hook` call `decision_for`
/// unconditionally whenever `try_fuse_create` returns `None`, regardless of
/// *why* it returned `None`, so without this guard an open already recorded
/// there would be counted a second time here.
///
/// Cheap when disabled: the caller passes an already-decoded `path` rather
/// than this function re-decoding `oa` itself (see `tag_under_root`'s doc
/// comment for why re-decoding independently, per caller, is the thing to
/// avoid), and `note_open_outcome` itself is a no-op when stats are disabled.
fn note_decision_outcome(
    path: Option<&str>,
    already: bool,
    outcome: crate::hookstats::OpenOutcome,
) {
    if already || !crate::hookstats::enabled() {
        return;
    }
    if let Some(p) = path {
        crate::hookstats::note_open_outcome(outcome, p);
    }
}

/// Same purpose as [`note_decision_outcome`], specialised for
/// `Decision::PassThrough`: the brief scopes that outcome to opens **under a
/// managed root** — a `PassThrough` decision also fires for paths outside
/// every root (the ordinary case, e.g. `kernel32.dll`), and counting those
/// would drown the fall-through signal in background noise unrelated to any
/// bypass. `path_is_ours` is the one helper that already answers "under a
/// managed root" correctly for both the engine's and the FUSE client's
/// notions of the root (see its own doc comment).
///
/// Caller's responsibility: hold a `vfs_redirect::UncachedScope` around this
/// call if `path` came from an OS-consulted decode — `path_is_ours` reaches
/// the same cached `RootMap::under_root` `decision_for` does.
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

/// Try director FUSE OPEN for paths under the managed root. Returns Some(status)
/// when the fuse client handled the call (success or hard failure under root).
/// An under-root open needs the ring WRITE path when write access or a
/// create/overwrite disposition is present (SUPERSEDE/CREATE/OPEN_IF/
/// OVERWRITE[_IF]).
///
/// `FILE_OPEN_IF` (3) belongs in the disposition set alongside the other
/// creating dispositions: it may create the path (no different from
/// `FILE_CREATE`/`FILE_SUPERSEDE` in that respect), so a caller that asks
/// for it with only read access must still route through the write path —
/// otherwise a create-if-absent read open is treated as a plain read, which
/// reports `ST_NOT_FOUND` instead of creating the file, on an absent path.
/// (Before gate 4's Task 5 that miss also *fell through* to the shim-local
/// overlay, so the misclassification silently "worked"; now it is a sealed
/// failure, which is the same reason getting this predicate right matters
/// more, not less.)
fn is_write_open(access: u32, disposition: u32) -> bool {
    (access & vfs_redirect::WRITE_ACCESS) != 0 || matches!(disposition, 0 | 2 | 3 | 4 | 5)
}

/// True for NT's append-only access grant: `FILE_APPEND_DATA` without
/// `FILE_WRITE_DATA`. A real file object forces every write on such a handle
/// to the current end of file, ignoring any caller-supplied offset, because
/// the kernel enforces it at the file-object level. A synthetic handle has no
/// kernel object to do that for it, so the open path has to seed the tracked
/// position at the file's current size (`fuse_synth::open_fuse_at_ex`) and
/// `write_hook` has to keep pinning it there — see both for the other half.
///
/// `Rust`'s `OpenOptions::append(true)` without `.write(true)` — the fixture's
/// reopen-for-append step — requests exactly this access, which is how the
/// gap surfaced: an append reopen's first write landed at position 0 (the
/// hardcoded initial value) and silently overwrote the file's existing bytes
/// instead of extending it.
///
/// `GENERIC_WRITE` must count as full write access here too, same as
/// `is_write_open`'s `WRITE_ACCESS`: a caller requesting
/// `GENERIC_WRITE | FILE_APPEND_DATA` wants ordinary positional writes plus
/// append, not append-only — checking only the literal `FILE_WRITE_DATA` bit
/// missed that, because `GENERIC_WRITE` is a generic right that implies
/// `FILE_WRITE_DATA` without necessarily carrying its specific bit set in the
/// raw mask this hook observes.
fn is_append_only(access: u32) -> bool {
    use vfs_redirect::{FILE_APPEND_DATA, FILE_WRITE_DATA, GENERIC_WRITE};
    access & FILE_APPEND_DATA != 0 && access & (FILE_WRITE_DATA | GENERIC_WRITE) == 0
}

/// Map an NT create-disposition to the ring's `OPEN_CREATE`/`OPEN_EXCL`/
/// `OPEN_TRUNC` bits (`OPEN_WRITE` itself is added by the caller). Forwarding
/// this is what closes the gap Task 6 found: without it every brand-new file
/// gets `ST_NOT_FOUND` from the director regardless of disposition. That used
/// to fall through to the shim-local overlay redirect and so merely misplace
/// the bytes; since gate 4's Task 5 sealed that fall-through it would instead
/// fail every create outright, so a mistake in this mapping is now a game that
/// cannot write at all rather than one that writes to the wrong place.
///
/// Verified against NT `CreateDisposition` semantics one value at a time
/// (a prior draft of this mapping under-specified two of the six):
/// - `FILE_SUPERSEDE` (0): create if absent, replace if present -> needs
///   **both** `OPEN_CREATE` and `OPEN_TRUNC` — a `OPEN_TRUNC`-only mapping
///   fails `ST_NOT_FOUND` on an absent file, which is exactly the bug this
///   function exists to close.
/// - `FILE_OPEN` (1): open only, must fail if absent -> no flags.
/// - `FILE_CREATE` (2): create only, must fail if present -> `OPEN_CREATE |
///   OPEN_EXCL`.
/// - `FILE_OPEN_IF` (3): open if present (no data loss), create if absent ->
///   `OPEN_CREATE` alone (the provider's `OPEN_CREATE` is a no-op on an
///   existing file — it does not also truncate).
/// - `FILE_OVERWRITE` (4): must already exist, truncate -> `OPEN_TRUNC` alone
///   (no `OPEN_CREATE`, so an absent file still fails `ST_NOT_FOUND`, matching
///   "fail if it does not exist").
/// - `FILE_OVERWRITE_IF` (5): overwrite if present, create if absent -> needs
///   **both**, same as `FILE_SUPERSEDE` — this is the case the brief already
///   flagged for a re-check.
///
/// Cross-checked against `DiskProvider::open` (`disk.rs`), which folds these
/// straight into `OpenOptions::create/create_new/truncate`, and the
/// conformance fixture's `open`, which create-if-absent-then-truncate in that
/// order — both agree with the mapping above.
fn open_create_flags(disposition: u32) -> u32 {
    use vfs_protocol::{OPEN_CREATE, OPEN_EXCL, OPEN_TRUNC};
    match disposition {
        0 => OPEN_CREATE | OPEN_TRUNC,
        2 => OPEN_CREATE | OPEN_EXCL,
        3 => OPEN_CREATE,
        4 => OPEN_TRUNC,
        5 => OPEN_CREATE | OPEN_TRUNC,
        _ => 0, // FILE_OPEN (1), and anything unrecognized.
    }
}

/// True for the dispositions where a write-flavoured open that turns out to
/// name an existing **directory** is a legitimate directory open rather than
/// a failed file create.
///
/// `is_write_open`'s `WRITE_ACCESS` includes `0x0002 | 0x0004`, which on a
/// *directory* handle are `FILE_ADD_FILE` and `FILE_ADD_SUBDIRECTORY`, not
/// `FILE_WRITE_DATA`/`FILE_APPEND_DATA`. The bits are identical and nothing
/// in the mask distinguishes them, so every `FILE_FLAG_BACKUP_SEMANTICS` open
/// asking for write access on a directory arrives as a write, gets routed to
/// `Provider::open(OPEN_WRITE)`, and fails: `DiskProvider::open` opens
/// read+write, which a directory refuses. Since gate 4's Task 5 that failure
/// is no longer papered over by the fall-through — the caller now gets
/// `STATUS_UNSUCCESSFUL` (`ERROR_GEN_FAILURE`) for an operation NTFS answers
/// without complaint.
///
/// Only `FILE_OPEN` and `FILE_OPEN_IF` qualify. The other four
/// (`SUPERSEDE`/`CREATE`/`OVERWRITE`/`OVERWRITE_IF`) all intend to create or
/// replace, and NT answers those against an existing directory with a
/// collision or `STATUS_FILE_IS_A_DIRECTORY` — handing back a directory
/// handle there would turn a refused file create into a silent success.
/// Directory *creates* never reach this at all: `try_fuse_mkdir` runs first
/// and takes `FILE_DIRECTORY_FILE` with a creating disposition.
fn dir_open_downgrades(disposition: u32) -> bool {
    matches!(disposition, 1 | 3)
}

/// True for the three dispositions whose successful `IoStatusBlock`
/// `Information` depends on whether the path already existed
/// (`FILE_SUPERSEDE`/`FILE_OPEN_IF`/`FILE_OVERWRITE_IF`) — see
/// `disposition_information`. The other three have one fixed outcome and
/// need no probe.
fn disposition_needs_existence_probe(disposition: u32) -> bool {
    matches!(disposition, 0 | 3 | 5)
}

/// The correct `IoStatusBlock.Information` for a *successful* create/open,
/// given the NT create-disposition and (for the three dispositions where it
/// matters) whether the path existed before the call.
///
/// `create_hook` used to hardcode `FILE_OPENED` here unconditionally, which
/// was invisible while every write fell through to a real file (whose kernel
/// FCB reports this correctly on its own) — only reachable now that writes
/// succeed through the director. Kernel32's `ERROR_ALREADY_EXISTS` signalling
/// for `CREATE_ALWAYS` (`FILE_SUPERSEDE`) / `OPEN_ALWAYS` (`FILE_OPEN_IF`)
/// reads exactly this field, so getting it wrong is not cosmetic.
///
/// NT's own table:
/// - `FILE_OPEN` (1): always `FILE_OPENED` — must already exist.
/// - `FILE_CREATE` (2): always `FILE_CREATED` — must not have existed
///   (`OPEN_EXCL` already enforces this; success implies "created").
/// - `FILE_OVERWRITE` (4): always `FILE_OVERWRITTEN` — must already exist.
/// - `FILE_SUPERSEDE` (0), `FILE_OPEN_IF` (3), `FILE_OVERWRITE_IF` (5):
///   outcome depends on whether the path existed — this is exactly why
///   `disposition_needs_existence_probe` singles these three out.
fn disposition_information(disposition: u32, existed_before: bool) -> usize {
    use crate::ntdef::{FILE_CREATED, FILE_OPENED, FILE_OVERWRITTEN, FILE_SUPERSEDED};
    match disposition {
        0 => {
            if existed_before {
                FILE_SUPERSEDED
            } else {
                FILE_CREATED
            }
        }
        2 => FILE_CREATED,
        3 => {
            if existed_before {
                FILE_OPENED
            } else {
                FILE_CREATED
            }
        }
        4 => FILE_OVERWRITTEN,
        5 => {
            if existed_before {
                FILE_OVERWRITTEN
            } else {
                FILE_CREATED
            }
        }
        _ => FILE_OPENED, // FILE_OPEN (1), and anything unrecognized.
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
    // physical open before returning `None`. Exactly one site does that now —
    // the write fallback behind `allow_disk_fallthrough`; the DRM exception
    // was the other, and gate 5 Task 4 deleted it.
    // Both callers (`create_hook`/`open_hook`) still unconditionally call
    // `decision_for` afterward for the actual routing decision, and without
    // this out-param that second classification would double-count the same
    // open — see the callers' use of it for the full argument.
    outcome_recorded: &mut bool,
) -> Option<NTSTATUS> {
    let client = crate::director::global()?;
    let path = path?.to_string();
    let (root, vp) = client.route(&path)?;
    let vp = vp.as_str();

    // No basename exceptions: `steam_appid.txt`, `SkyrimSELauncher.exe`,
    // `steam_api{,64}.dll` and `SkyrimSE.exe` are served or sealed like any other path
    // under the root, so nothing under a managed root reaches the real disk by name.
    // `OpenOutcome::FellThroughDrmException` stays in the enum and reads zero.
    // See docs/shim-invariants.md, "Sealed root: opens".
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    drm_exe_trace(&path, unsafe { fuse_root_directory(oa) }, write);

    // Every under-root open, read and write, goes through the director, and every
    // answer it gives, failures included, is this function's answer: no decision below
    // returns `None` except behind the `allow_disk_fallthrough` opt-out. The one other
    // `None` (`open_fuse_at_ex(...)?` on a poisoned synth table) is not reachable:
    // nothing inside those critical sections can unwind. `contain_panic` would not
    // make it safe, since the guard's drop has already poisoned the lock. Re-check
    // that argument if `fuse_synth` grows a fallible or reentrant operation under its
    // locks.
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
            let h = crate::fuse_synth::open_fuse_at_ex(
                resp.fh,
                resp.size,
                resp.is_dir,
                Some(path.clone()),
                append_only,
            )?;
            // Every file handle joins the read cache's view of its file: a
            // write or mutable open drops what it holds of it; an immutable
            // read open may be served from it.
            if let Some(cache) = crate::read_cache::register(root.0, vp, &resp, write) {
                crate::fuse_synth::set_cache(h, cache);
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
            if let Ok(mut t) = PATH_TABLE.lock() {
                t.insert(h, path.clone());
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
        // through to `decision_for` here; it no longer does.
        // See docs/shim-invariants.md, "Sealed root: statuses".
        Err(st) if st == vfs_protocol::ST_NOT_FOUND => {
            if allow_disk_fallthrough() {
                // The root is unsealed by operator opt-in. A write really does
                // fall through here, so it is still recorded as one — this is
                // the last site that can move `FellThroughWriteFallback` off
                // zero, and a live report showing it non-zero now means
                // exactly one thing: this switch is on. (Reads stay
                // unrecorded here, as before: `decision_for` classifies them
                // a few lines up the stack.)
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
        // guard below and fell through to the shim-local overlay, which
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
            let h = crate::fuse_synth::open_fuse(0, 0, true)?;
            if !file_handle.is_null() {
                // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
                unsafe {
                    *file_handle = h as HANDLE;
                }
            }
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, FILE_CREATED) };
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
                    let h = crate::fuse_synth::open_fuse(0, 0, true)?;
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
    // `RootMap`-backed decision made below with `path` (`decision_for`, and
    // `path_is_ours` via `tag_under_root`/`record_path`/
    // `note_passthrough_outcome`) must not be cached under it. See
    // `vfs_redirect::UncachedScope`'s doc comment.
    let _uncached_guard = os_consulted.then(vfs_redirect::UncachedScope::enter);

    // Directory create under the managed root → ring OP_MKDIR (must precede the
    // generic file open below, which would otherwise create a FILE named as the
    // directory via the write-create path).
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    if let Some(st) = unsafe { try_fuse_mkdir(file_handle, path, iosb, opts, disp) } {
        return st;
    }
    // Prefer director FUSE for managed-root content (no in-shim zipserve).
    match path {
        Some(p) => crate::hookstats::note_passthrough(p),
        // An open we cannot decode is an open we cannot serve. If the masters
        // are hiding anywhere, it is here.
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        None => crate::hookstats::note_undecodable(unsafe { object_name_str(oa) }.as_deref()),
    }
    // Set by `try_fuse_create` when it already recorded an outcome (the write
    // fallback — the DRM exception was the other one and is gone) for this
    // open before returning `None` — see `note_decision_outcome` below for why
    // that must suppress the `decision_for`-based recording that always runs
    // next.
    let mut outcome_recorded = false;
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
        _hs.mark_rooted();
        // FILE_SYNCHRONOUS_IO_ALERT | FILE_SYNCHRONOUS_IO_NONALERT. Absent means
        // the caller intends asynchronous completion, which a synthetic handle
        // cannot deliver by APC or completion port.
        crate::hookstats::note_open_sync(opts & 0x0000_0030 != 0);
        return st;
    }
    let decision = decision_for(path, access, disp);
    let is_passthrough = matches!(&decision, Some(Decision::PassThrough));
    match decision {
        Some(Decision::Redirect { target_nt }) => {
            note_decision_outcome(
                path,
                outcome_recorded,
                crate::hookstats::OpenOutcome::FellThroughRedirect,
            );
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            let new_oa = match unsafe { redirected_oa(oa, &target_nt) } {
                Ok(o) => o,
                Err(st) => return st,
            };
            // SAFETY: the original NT function, called with valid NT arguments.
            let status = unsafe {
                tramp(
                    file_handle,
                    access,
                    new_oa.as_ptr(),
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
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { record_identity(file_handle, path, status) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { record_path(file_handle, path, status) };
            status
        }
        Some(Decision::Deny) => {
            note_decision_outcome(
                path,
                outcome_recorded,
                crate::hookstats::OpenOutcome::Denied,
            );
            STATUS_OBJECT_NAME_NOT_FOUND
        }
        Some(Decision::PassThrough) | None => {
            if is_passthrough {
                note_passthrough_outcome(path, outcome_recorded);
            }
            // Never pass a FUSE RootDirectory to the kernel (invalid handle):
            // rebuild an absolute OA from the decoded path instead. The DRM
            // exceptions were this arm's reason to exist and are gone (gate 5,
            // Task 4); see `tramp_create_abs` for what still reaches it.
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            if unsafe { fuse_root_directory(oa) } {
                if let Some(path) = path {
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    let status = unsafe {
                        tramp_create_abs(
                            tramp,
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
                            path,
                        )
                    };
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    unsafe { tag_under_root(file_handle, Some(path), status) };
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    unsafe { record_path(file_handle, Some(path), status) };
                    return status;
                }
                return STATUS_OBJECT_NAME_NOT_FOUND;
            }
            // SAFETY: the original NT function, called with valid NT arguments.
            let status = unsafe {
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
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { tag_under_root(file_handle, path, status) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { record_path(file_handle, path, status) };
            status
        }
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

/// Open via trampoline with an absolute NT path and **null** RootDirectory.
///
/// Required when the original OA had a FUSE synthetic RootDirectory, which is
/// invalid to the kernel, but the open is nonetheless falling through to it.
///
/// **The four DRM exceptions used to be the reason this existed** and are gone
/// (gate 5, Task 4). What is left is the narrow disagreement case: a synthetic
/// `RootDirectory` whose `PATH_TABLE` entry resolves to a path that
/// `FuseClient::vpath_under_root` does *not* place under any root, so
/// `try_fuse_create` declined it. That is a genuine inconsistency between the
/// two root notions rather than a policy, and passing the synthetic handle to
/// the kernel would fail with a misleading status, so the absolute rebuild
/// stays. Arity mirrors `NtCreateFile` exactly; it is not ours to reduce.
#[allow(clippy::too_many_arguments)]
unsafe fn tramp_create_abs(
    tramp: NtCreateFileFn,
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
    abs_path: &str,
) -> NTSTATUS {
    let nt = to_nt_path(abs_path);
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    let new_oa = match unsafe { redirected_oa(oa, &nt) } {
        Ok(o) => o,
        Err(st) => return st,
    };
    // SAFETY: the original NT function, called with valid NT arguments.
    unsafe {
        tramp(
            file_handle,
            access,
            new_oa.as_ptr(),
            iosb,
            alloc,
            attrs,
            share,
            disp,
            opts,
            ea,
            ealen,
        )
    }
}

/// Arity mirrors `NtOpenFile` exactly; it is not ours to reduce.
#[allow(clippy::too_many_arguments)]
unsafe fn tramp_open_abs(
    tramp: NtOpenFileFn,
    file_handle: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    share: u32,
    opts: u32,
    abs_path: &str,
) -> NTSTATUS {
    let nt = to_nt_path(abs_path);
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    let new_oa = match unsafe { redirected_oa(oa, &nt) } {
        Ok(o) => o,
        Err(st) => return st,
    };
    // SAFETY: the original NT function, called with valid NT arguments.
    unsafe { tramp(file_handle, access, new_oa.as_ptr(), iosb, share, opts) }
}

/// `NtOpenFile` hook. Mirrors `create_hook` (redirect / deny / pass-through +
/// dir tagging) for the open path that Rust `std` and many Win32 callers use to
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
    // Decode once for the whole call — see `create_hook` and `tag_under_root`'s
    // doc comment for why, and for what the `UncachedScope` guard is for.
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    let decoded = match unsafe { path_of_tracked(oa) } {
        Ok(d) => d,
        Err(st) => return st,
    };
    let path: Option<&str> = decoded.as_ref().map(|d| d.path.as_str());
    let os_consulted = decoded.as_ref().is_some_and(|d| d.os_consulted);
    let _uncached_guard = os_consulted.then(vfs_redirect::UncachedScope::enter);

    match path {
        Some(p) => crate::hookstats::note_passthrough(p),
        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
        None => crate::hookstats::note_undecodable(unsafe { object_name_str(oa) }.as_deref()),
    }
    // Set by `try_fuse_create` when it already recorded an outcome (the write
    // fallback — the DRM exception was the other one and is gone) for this
    // open before returning `None` — see `note_decision_outcome` for why that
    // must suppress the `decision_for`-based recording that always runs next.
    let mut outcome_recorded = false;
    // NtOpenFile has no disposition — it always opens existing (FILE_OPEN). Pass
    // FILE_OPEN (1), NOT 0: 0 is FILE_SUPERSEDE, which is in is_write_open's
    // create/overwrite set and would misclassify every open as a write.
    // create_flags is always 0 here: an open-only call never creates,
    // truncates, or excludes.
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    if let Some(st) = unsafe {
        try_fuse_create(
            file_handle,
            oa,
            path,
            iosb,
            is_write_open(access, vfs_redirect::FILE_OPEN),
            vfs_redirect::FILE_OPEN,
            0,
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
        _hs.mark_rooted();
        return st;
    }
    // NtOpenFile has no disposition; it always opens existing (FILE_OPEN).
    let decision = decision_for(path, access, vfs_redirect::FILE_OPEN);
    let is_passthrough = matches!(&decision, Some(Decision::PassThrough));
    match decision {
        Some(Decision::Redirect { target_nt }) => {
            note_decision_outcome(
                path,
                outcome_recorded,
                crate::hookstats::OpenOutcome::FellThroughRedirect,
            );
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            let new_oa = match unsafe { redirected_oa(oa, &target_nt) } {
                Ok(o) => o,
                Err(st) => return st,
            };
            // SAFETY: the original NT function, called with valid NT arguments.
            let status = unsafe { tramp(file_handle, access, new_oa.as_ptr(), iosb, share, opts) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { record_identity(file_handle, path, status) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { record_path(file_handle, path, status) };
            status
        }
        Some(Decision::Deny) => {
            note_decision_outcome(
                path,
                outcome_recorded,
                crate::hookstats::OpenOutcome::Denied,
            );
            STATUS_OBJECT_NAME_NOT_FOUND
        }
        Some(Decision::PassThrough) | None => {
            if is_passthrough {
                note_passthrough_outcome(path, outcome_recorded);
            }
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            if unsafe { fuse_root_directory(oa) } {
                if let Some(path) = path {
                    let status =
                        // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                        unsafe { tramp_open_abs(tramp, file_handle, access, oa, iosb, share, opts, path) };
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    unsafe { tag_under_root(file_handle, Some(path), status) };
                    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
                    unsafe { record_path(file_handle, Some(path), status) };
                    return status;
                }
                return STATUS_OBJECT_NAME_NOT_FOUND;
            }
            // SAFETY: the original NT function, called with valid NT arguments.
            let status = unsafe { tramp(file_handle, access, oa, iosb, share, opts) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { tag_under_root(file_handle, path, status) };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { record_path(file_handle, path, status) };
            status
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Fix 8: two disposition-classification bugs.

    /// `GENERIC_WRITE | FILE_APPEND_DATA` is ordinary write-plus-append
    /// access, not append-only: `GENERIC_WRITE` already grants full
    /// positional write. Before the fix, `is_append_only` checked only the
    /// literal `FILE_WRITE_DATA` bit (0x0002), which `GENERIC_WRITE`
    /// (0x4000_0000) does not itself set in the raw mask this hook observes
    /// — so this combination was misclassified as append-only, which would
    /// have pinned every write to EOF regardless of the caller's offset.
    #[test]
    fn generic_write_with_append_data_is_not_append_only() {
        use vfs_redirect::{FILE_APPEND_DATA, GENERIC_WRITE};
        assert!(!is_append_only(GENERIC_WRITE | FILE_APPEND_DATA));
    }

    /// The genuine append-only shape — `FILE_APPEND_DATA` with neither
    /// `FILE_WRITE_DATA` nor `GENERIC_WRITE` — must still be classified as
    /// append-only. `Rust`'s `OpenOptions::append(true)` (without
    /// `.write(true)`) requests exactly this.
    #[test]
    fn append_data_alone_is_append_only() {
        use vfs_redirect::FILE_APPEND_DATA;
        assert!(is_append_only(FILE_APPEND_DATA));
    }

    /// `FILE_WRITE_DATA` set explicitly alongside `FILE_APPEND_DATA` is full
    /// write access, not append-only — unchanged by the fix, kept here so a
    /// future edit cannot silently invert it.
    #[test]
    fn explicit_write_data_with_append_data_is_not_append_only() {
        use vfs_redirect::{FILE_APPEND_DATA, FILE_WRITE_DATA};
        assert!(!is_append_only(FILE_WRITE_DATA | FILE_APPEND_DATA));
    }

    /// `FILE_OPEN_IF` (3) may create the path, exactly like `FILE_CREATE`/
    /// `FILE_SUPERSEDE`/`FILE_OVERWRITE_IF` — so it must count as a write
    /// open even with only read access requested. Before the fix, disposition
    /// 3 was missing from `is_write_open`'s disposition set, so a
    /// create-if-absent read open (read access + `FILE_OPEN_IF`) was treated
    /// as a plain read and never reached the director's create path on an
    /// absent file.
    #[test]
    fn file_open_if_with_read_only_access_is_a_write_open() {
        const GENERIC_READ: u32 = 0x8000_0000;
        const FILE_OPEN_IF: u32 = 3;
        assert!(is_write_open(GENERIC_READ, FILE_OPEN_IF));
    }

    /// Every disposition NT itself can create through must be a write open
    /// regardless of the access mask; `FILE_OPEN` (1) is the sole disposition
    /// that depends on the access mask alone.
    #[test]
    fn every_creating_disposition_is_a_write_open_even_with_read_only_access() {
        const GENERIC_READ: u32 = 0x8000_0000;
        for disposition in [0u32, 2, 3, 4, 5] {
            assert!(
                is_write_open(GENERIC_READ, disposition),
                "disposition {disposition} must be a write open"
            );
        }
        const FILE_OPEN: u32 = 1;
        assert!(
            !is_write_open(GENERIC_READ, FILE_OPEN),
            "FILE_OPEN with only read access must not be a write open"
        );
    }

    /// Gate 4, Task 6. Only the two non-creating dispositions may hand back a
    /// directory handle when a write-flavoured open turns out to name a
    /// directory. Widening this to the creating four would turn "you cannot
    /// create a file where a directory already is" — which NT answers with a
    /// collision or `STATUS_FILE_IS_A_DIRECTORY` — into a silent success
    /// handing the caller a directory handle it never asked for.
    #[test]
    fn only_non_creating_dispositions_downgrade_a_directory_open() {
        assert!(
            dir_open_downgrades(1),
            "FILE_OPEN opens an existing directory"
        );
        assert!(
            dir_open_downgrades(3),
            "FILE_OPEN_IF opens an existing directory"
        );
        for disposition in [0u32, 2, 4, 5] {
            assert!(
                !dir_open_downgrades(disposition),
                "disposition {disposition} intends to create or replace a file; a directory \
                 handle is not an acceptable answer to it"
            );
        }
    }

    // --- Fix 7: per-disposition IoStatusBlock.Information.

    #[test]
    fn disposition_information_matches_nt_semantics() {
        use crate::ntdef::{FILE_CREATED, FILE_OPENED, FILE_OVERWRITTEN, FILE_SUPERSEDED};

        // FILE_SUPERSEDE (0): existed -> SUPERSEDED, absent -> CREATED.
        assert_eq!(disposition_information(0, true), FILE_SUPERSEDED);
        assert_eq!(disposition_information(0, false), FILE_CREATED);
        // FILE_OPEN (1): always OPENED.
        assert_eq!(disposition_information(1, true), FILE_OPENED);
        assert_eq!(disposition_information(1, false), FILE_OPENED);
        // FILE_CREATE (2): always CREATED.
        assert_eq!(disposition_information(2, true), FILE_CREATED);
        assert_eq!(disposition_information(2, false), FILE_CREATED);
        // FILE_OPEN_IF (3): existed -> OPENED, absent -> CREATED.
        assert_eq!(disposition_information(3, true), FILE_OPENED);
        assert_eq!(disposition_information(3, false), FILE_CREATED);
        // FILE_OVERWRITE (4): always OVERWRITTEN.
        assert_eq!(disposition_information(4, true), FILE_OVERWRITTEN);
        assert_eq!(disposition_information(4, false), FILE_OVERWRITTEN);
        // FILE_OVERWRITE_IF (5): existed -> OVERWRITTEN, absent -> CREATED.
        assert_eq!(disposition_information(5, true), FILE_OVERWRITTEN);
        assert_eq!(disposition_information(5, false), FILE_CREATED);
    }

    #[test]
    fn only_the_three_conditional_dispositions_need_an_existence_probe() {
        for d in [0u32, 3, 5] {
            assert!(disposition_needs_existence_probe(d), "disposition {d}");
        }
        for d in [1u32, 2, 4] {
            assert!(!disposition_needs_existence_probe(d), "disposition {d}");
        }
    }
}
