//! Raw Win32 lookups behind path canonicalisation: which drive letters exist
//! right now, what NT device name and volume-GUID mount point each currently
//! resolves to, and expanding a short (8.3) name — or any other alternate
//! spelling of the same file (junction, hardlink, `subst`/mapped drive) — to
//! its real path. All Win32 FFI for this is confined here; every caller gets
//! back plain, owned `String`s.
#![allow(unsafe_code)]

use windows_sys::Wdk::Storage::FileSystem::REPARSE_DATA_BUFFER;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFinalPathNameByHandleW, GetLogicalDrives, GetLongPathNameW,
    GetShortPathNameW, GetVolumeNameForVolumeMountPointW, QueryDosDeviceW,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
use windows_sys::Win32::System::SystemServices::{IO_REPARSE_TAG_MOUNT_POINT, IO_REPARSE_TAG_SYMLINK};
use windows_sys::Win32::System::IO::DeviceIoControl;

/// What one currently-mounted drive letter resolves to at the NT layer.
///
/// Invariant: `device_name` always names a genuine NT device-namespace
/// object (`is_device_namespace_name(&device_name)` is always `true`) —
/// never a `subst`/directory-alias target such as `\??\C:\Windows`. See
/// `drive_mappings` for why that distinction is load-bearing, not cosmetic.
#[derive(Debug, Clone)]
pub struct DriveMapping {
    pub drive: char,
    /// The NT device name the drive letter is a `\DosDevices` symbolic link
    /// to, e.g. `\Device\HarddiskVolume3` (`QueryDosDeviceW`).
    pub device_name: String,
    /// The drive's volume-GUID mount point, in the Win32 spelling
    /// `GetVolumeNameForVolumeMountPointW` returns —
    /// `\\?\Volume{guid}\`, trailing separator included. `None` if the
    /// drive has no such mount point (e.g. some network drives). Callers
    /// that need the NT spelling a real open actually presents must convert
    /// this themselves; this module only reports what the OS said.
    pub volume_guid_win32: Option<String>,
}

/// Every drive letter Windows currently reports as in use that genuinely
/// needs device-namespace resolution, with each one's device name and
/// volume-GUID mount point. Cheap enough to call once per session, but the
/// result is a snapshot: a drive mounted or unmounted afterwards is not
/// reflected until this is called again.
///
/// A drive whose `QueryDosDeviceW` target is not a `\Device\...` name is
/// skipped entirely rather than included as-is. The case that matters:
/// `subst Z: C:\Games` (no elevation required, takes effect immediately in
/// the calling session) makes `QueryDosDeviceW("Z:")` return
/// `\??\C:\Games` — a drive alias, not a device. If that were registered in
/// `VolumeMap` the way a real device is, its prefix would match any path
/// under `C:\Games` (including a path already spelled correctly as
/// `C:\...` or `\??\C:\...`) and rewrite it to `Z:`, which is not merely an
/// unmapped-device miss but an active regression: the path canonicalised
/// correctly *before* this map had an opinion about it. A `subst`
/// alias resolves the way any other alias to a path does — through
/// final-path resolution — not through this device-namespace map.
pub fn drive_mappings() -> Vec<DriveMapping> {
    let mask = logical_drives_mask();
    (0..26u32)
        .filter(|bit| mask & (1 << bit) != 0)
        .filter_map(|bit| {
            let drive = (b'A' + bit as u8) as char;
            let device_name = query_dos_device(drive)?;
            if !is_device_namespace_name(&device_name) {
                return None;
            }
            Some(DriveMapping { drive, device_name, volume_guid_win32: volume_guid_for_drive(drive) })
        })
        .collect()
}

/// Whether `device_name` (as `QueryDosDeviceW` returns it for a drive
/// letter) genuinely names an NT device-namespace object (`\Device\...`)
/// rather than a `subst`/directory-alias target (`\??\C:\Windows` and the
/// like). Public so a caller building a `VolumeMap` from raw drive data —
/// or a regression test reproducing a `subst` shape without actually
/// running `subst` — can apply the exact guard `drive_mappings` relies on.
pub fn is_device_namespace_name(device_name: &str) -> bool {
    device_name.get(..8).is_some_and(|p| p.eq_ignore_ascii_case(r"\Device\"))
}

/// Bitmask of drive letters currently in use, bit 0 = A ... bit 25 = Z.
fn logical_drives_mask() -> u32 {
    // SAFETY: FFI. No arguments, no preconditions.
    unsafe { GetLogicalDrives() }
}

/// The NT device name a drive letter is currently a symbolic link to (e.g.
/// `C:` -> `\Device\HarddiskVolume3`), or `None` if the letter has no
/// `\DosDevices` mapping right now (`GetLogicalDrives` said it existed, but
/// it was unmounted in the meantime, or is a symlink-less pseudo-drive).
fn query_dos_device(drive: char) -> Option<String> {
    let device = to_wide(&format!("{drive}:"));
    let mut buf = vec![0u16; 4096];
    // SAFETY: FFI. `device` is a valid NUL-terminated UTF-16 pointer for the
    // call; `buf` is valid for `buf.len()` `u16`s, which is what `ucchmax`
    // declares as the buffer's capacity in `u16` units.
    let len = unsafe { QueryDosDeviceW(device.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if len == 0 {
        return None;
    }
    // QueryDosDeviceW can return more than one NUL-separated target for a
    // drive with several mappings (e.g. `subst` stacked over a real drive);
    // the first is the current one.
    let first = buf.split(|&c| c == 0).next().unwrap_or(&[]);
    if first.is_empty() {
        None
    } else {
        Some(String::from_utf16_lossy(first))
    }
}

/// The volume-GUID mount point a drive letter currently resolves to, in the
/// Win32 spelling `GetVolumeNameForVolumeMountPointW` returns.
fn volume_guid_for_drive(drive: char) -> Option<String> {
    let mount_point = to_wide(&format!("{drive}:\\"));
    // MSDN: a buffer of 50 characters is guaranteed sufficient for the fixed
    // `\\?\Volume{guid}\` form; comfortably rounded up.
    let mut buf = vec![0u16; 130];
    // SAFETY: FFI. `mount_point` is a valid NUL-terminated UTF-16 pointer,
    // ending in a separator as this API requires; `buf` is valid for
    // `buf.len()` `u16`s, matching `cchbufferlength`.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(mount_point.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
    };
    if ok == 0 {
        return None;
    }
    Some(wide_buf_to_string(&buf))
}

/// Expand every 8.3 short-name component in `path` to its long form via
/// `GetLongPathNameW`. This is the fallback for a path nothing can be
/// opened at (see [`final_path_for_open`], which is authoritative — and
/// also collapses junctions, hardlinks, and `subst`/mapped drives — but
/// needs a real, openable path to work from).
pub fn expand_long_path(path: &str) -> Option<String> {
    let wide = to_wide(path);
    grow_to_fit(|buf| {
        // SAFETY: FFI. `wide` is a valid NUL-terminated UTF-16 pointer for
        // the call; `buf` is valid for `buf.len()` `u16`s, matching
        // `cchbuffer`.
        unsafe { GetLongPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) }
    })
}

/// The short (8.3) form of `path`, if the filesystem has one. Exposed for
/// tests that need to construct a real short name to round-trip through
/// [`expand_long_path`] / [`final_path_for_open`]; not used by the
/// production canonicalisation path.
pub fn short_path_name(path: &str) -> Option<String> {
    let wide = to_wide(path);
    grow_to_fit(|buf| {
        // SAFETY: FFI. Same contract as `GetLongPathNameW` above.
        unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) }
    })
}

/// The authoritative real path of whatever `path` currently names: open a
/// query-only handle and delegate to [`final_path_for_handle`], which resolves
/// 8.3 short names, junctions, hardlinks, and `subst`/mapped drives all at
/// once — each is a different spelling of "what does this handle actually
/// point at", a question the OS already has to answer to service the open.
/// `None` if nothing could be opened at `path` (it may not exist, or name a
/// form this process cannot open); callers fall back to
/// [`expand_long_path`] in that case.
pub fn final_path_for_open(path: &str) -> Option<String> {
    let wide = to_wide(path);
    // SAFETY: FFI. `wide` is a valid NUL-terminated UTF-16 pointer for the
    // call. Desired access 0 ("query access only") plus full sharing means
    // this never contends with or blocks another opener of the same file.
    // `FILE_FLAG_BACKUP_SEMANTICS` is required to open a directory handle
    // and is harmless when `path` names a file.
    let handle: HANDLE = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            core::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            core::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: `handle` was just opened successfully above, is a valid open
    // handle, and is not used again after `final_path_for_handle` returns
    // except to close it immediately below.
    let result = unsafe { final_path_for_handle(handle) };
    // SAFETY: FFI. `handle` is the same valid handle opened above, closed
    // exactly once here on every path out of this function.
    unsafe {
        CloseHandle(handle);
    }
    result
}

/// The raw substitute-name target of a directory junction or symlink
/// reparse point, read directly from the reparse point's own on-disk
/// metadata — **never opening or following whatever it points at**. `None`
/// if `path` is not a reparse point, is a reparse point of some other kind
/// (a `subst`/mapped-drive-style directory alias behaves like a genuine
/// junction here — `IO_REPARSE_TAG_MOUNT_POINT` covers both — but a
/// deduplication or WOF-compressed file, an `AppExecLink`, or a cloud
/// storage placeholder like a OneDrive "online-only" file/folder is a
/// different reparse tag entirely and is deliberately left alone), or
/// cannot be read.
///
/// **This is deliberately not [`final_path_for_open`].** That function opens
/// (and therefore follows) whatever `path` names, which is the wrong tool
/// for scanning directories a real Windows profile accumulates rather than
/// ones the caller already knows are safe to enter: a junction whose target
/// device is offline, disconnected, or asleep blocks on the OS's own
/// device/network timeout (tens of seconds, once per such junction) before
/// `CreateFileW` returns at all. This is not a hypothetical — a session-start
/// ancestor scan of an ordinary user profile genuinely encounters several
/// real reparse points before it ever reaches anything project-specific:
/// Windows' own built-in legacy-compatibility junctions
/// (`AppData\Local\Application Data`, `<profile>\Cookies`,
/// `<profile>\SendTo`, `Users\All Users`, `C:\Documents and Settings`, and
/// more), which are normally fast to reject (an explicit ACL denies
/// traversal) but are not a bet worth making, and applications that redirect
/// their own AppData folder to another drive via a real junction. Reading
/// the reparse point's own metadata — stored in its own directory entry, on
/// the *reparse point's* volume, never the target's — touches only that one
/// local, already-open handle and cannot block on anything the target
/// implies. Found by reproduction, not assumed: the escape-matrix vector 7
/// closeout's own session-start scan hung on exactly this shape of junction
/// before this function replaced a `final_path_for_open`-based first draft.
pub fn reparse_point_target(path: &str) -> Option<String> {
    let wide = to_wide(path);
    // SAFETY: FFI. `FILE_FLAG_OPEN_REPARSE_POINT` is what makes this open
    // the reparse point itself rather than following it — the entire point
    // of this function. Desired access 0 (query only) plus full sharing,
    // same convention as `final_path_for_open`.
    let handle: HANDLE = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            core::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            core::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    let result = reparse_target_from_handle(handle);
    // SAFETY: FFI. `handle` is the same valid handle opened above, closed
    // exactly once here on every path out of this function.
    unsafe {
        CloseHandle(handle);
    }
    result
}

/// `MAXIMUM_REPARSE_DATA_BUFFER_SIZE` (16 KiB) — MSDN's own documented upper
/// bound on a reparse data buffer's size, so a single fixed-size buffer is
/// always sufficient for `FSCTL_GET_REPARSE_POINT`, no growth loop needed.
const MAX_REPARSE_DATA_BUFFER_SIZE: usize = 16 * 1024;

/// The `DeviceIoControl(FSCTL_GET_REPARSE_POINT)` + parsing behind
/// [`reparse_point_target`], split out so the handle-closing path above stays
/// a single, obvious `unsafe` block per operation.
fn reparse_target_from_handle(handle: HANDLE) -> Option<String> {
    let mut buf = vec![0u8; MAX_REPARSE_DATA_BUFFER_SIZE];
    let mut returned: u32 = 0;
    // SAFETY: FFI. `handle` is a valid, just-opened handle (caller's
    // contract); `buf` is valid for `buf.len()` bytes, matching
    // `noutbuffersize`; `returned` is a valid `u32` out-param. No input
    // buffer is needed for this control code.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_GET_REPARSE_POINT,
            core::ptr::null(),
            0,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf.len() as u32,
            &mut returned,
            core::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    // SAFETY: `buf` was populated by the OS above with at least
    // `size_of::<REPARSE_DATA_BUFFER>()` valid bytes (any smaller `returned`
    // would mean `DeviceIoControl` itself failed, already handled above) —
    // reading the fixed header through a raw pointer, never materializing a
    // `&REPARSE_DATA_BUFFER` that would claim the trailing flexible
    // `PathBuffer` is fully initialized (it is not, as declared — only
    // `returned` bytes are). `buf` is a `Vec<u8>`, which only guarantees
    // 1-byte alignment; the system allocator happens to hand back
    // 16-byte-aligned memory for an allocation this size today, but nothing
    // in `Vec`'s contract promises that, so `rdb_ptr` must be treated as
    // possibly misaligned for `REPARSE_DATA_BUFFER`'s `u16`/`u32` fields.
    // Every scalar field read below therefore goes through
    // `read_unaligned` on a pointer obtained via `addr_of!` (which itself
    // never dereferences), rather than an ordinary `(*ptr).field` place
    // expression, which would assume an alignment `Vec<u8>` does not give.
    let rdb_ptr = buf.as_ptr() as *const REPARSE_DATA_BUFFER;
    let tag = unsafe { core::ptr::addr_of!((*rdb_ptr).ReparseTag).read_unaligned() };
    let (name_offset, name_len, path_buf_ptr) = match tag {
        IO_REPARSE_TAG_MOUNT_POINT => {
            // SAFETY: `addr_of!` projects a field address without reading
            // through an intermediate reference, safe even though the
            // trailing `PathBuffer` is not fully initialized per its
            // nominal `[u16; 1]` declaration; `read_unaligned` on the
            // scalar fields does not require `mp` itself to be aligned.
            let mp = unsafe { core::ptr::addr_of!((*rdb_ptr).Anonymous.MountPointReparseBuffer) };
            let path_buf = unsafe { core::ptr::addr_of!((*mp).PathBuffer) as *const u16 };
            let offset = unsafe { core::ptr::addr_of!((*mp).SubstituteNameOffset).read_unaligned() };
            let len = unsafe { core::ptr::addr_of!((*mp).SubstituteNameLength).read_unaligned() };
            (offset, len, path_buf)
        }
        IO_REPARSE_TAG_SYMLINK => {
            let sl = unsafe { core::ptr::addr_of!((*rdb_ptr).Anonymous.SymbolicLinkReparseBuffer) };
            let path_buf = unsafe { core::ptr::addr_of!((*sl).PathBuffer) as *const u16 };
            let offset = unsafe { core::ptr::addr_of!((*sl).SubstituteNameOffset).read_unaligned() };
            let len = unsafe { core::ptr::addr_of!((*sl).SubstituteNameLength).read_unaligned() };
            (offset, len, path_buf)
        }
        // Any other reparse tag (cloud-file placeholders, deduplication,
        // WOF-compressed files, AppExecLink, ...) is out of scope — see this
        // function's own doc comment for why that is deliberate, not a gap.
        _ => return None,
    };
    read_utf16_bounded(&buf, path_buf_ptr, name_offset, name_len)
}

/// Read a UTF-16 substring of `len` bytes starting `offset` bytes after
/// `base`, bounds-checked against `buf` (the buffer `base` points into) —
/// `offset`/`len` come from the OS, but this never trusts them past the
/// buffer actually allocated for `FSCTL_GET_REPARSE_POINT`'s output.
fn read_utf16_bounded(buf: &[u8], base: *const u16, offset: u16, len: u16) -> Option<String> {
    let base_off = (base as usize).checked_sub(buf.as_ptr() as usize)?;
    let start = base_off.checked_add(offset as usize)?;
    let end = start.checked_add(len as usize)?;
    if end > buf.len() || !len.is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> =
        buf[start..end].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    Some(String::from_utf16_lossy(&units))
}

/// The authoritative real path an already-open `handle` names, via
/// `GetFinalPathNameByHandleW` directly on that handle — no new handle is
/// opened, and this function never closes `handle`; the caller keeps
/// ownership. This is the piece [`final_path_for_open`] wraps around its own
/// `CreateFileW`, exposed separately for a caller (e.g. the shim decoding an
/// `OBJECT_ATTRIBUTES.RootDirectory` it did not itself open) that already
/// holds a valid handle and has no path to reopen it by.
///
/// Returns the `VOLUME_NAME_DOS` form, which is `\\?\`-prefixed — the Win32
/// spelling, not the `\??\` a real NT open presents. Callers feeding this
/// into `canon`/`RootMap` must re-canonicalise it, exactly as
/// `vfs_redirect::expand_short_name`'s callers already do with
/// `final_path_for_open`'s identical result shape; hand-stripping the prefix
/// here instead would be the wrong fix.
///
/// `None` if `handle` is null, `INVALID_HANDLE_VALUE`, or does not support
/// the query (e.g. a pipe, or an object with no path).
///
/// # Safety
///
/// `handle` must be a currently-valid, open handle (or null / `INVALID_HANDLE_VALUE`,
/// both handled explicitly). Passing a closed, reused, or otherwise invalid
/// non-null handle value is undefined behaviour at the `GetFinalPathNameByHandleW`
/// FFI boundary, same as any other handle-consuming Win32 call.
pub unsafe fn final_path_for_handle(handle: HANDLE) -> Option<String> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return None;
    }
    grow_to_fit(|buf| {
        // SAFETY: FFI. `handle` validity is the caller's contract (see this
        // function's own safety doc); this call neither closes nor otherwise
        // consumes it. `buf` is valid for `buf.len()` `u16`s, matching
        // `cchfilepath`.
        unsafe {
            GetFinalPathNameByHandleW(handle, buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED)
        }
    })
}

/// Call `f` with a growing buffer until it reports success. Shared
/// convention of `GetLongPathNameW`, `GetShortPathNameW`, and
/// `GetFinalPathNameByHandleW`: `0` is failure; a return value strictly less
/// than the buffer length means the string (excluding its NUL) was copied
/// and fits; a return value `>=` the buffer length is the required buffer
/// size to retry with.
fn grow_to_fit(mut f: impl FnMut(&mut [u16]) -> u32) -> Option<String> {
    let mut cap = 260usize; // MAX_PATH; grows if that is not enough.
    for _ in 0..4 {
        let mut buf = vec![0u16; cap];
        let needed = f(&mut buf);
        if needed == 0 {
            return None;
        }
        if (needed as usize) < cap {
            buf.truncate(needed as usize);
            return Some(String::from_utf16_lossy(&buf));
        }
        cap = needed as usize + 1;
    }
    None
}

/// Convert a `&str` to a NUL-terminated UTF-16 buffer for the `*W` APIs.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

/// A fixed-size wide buffer written by a `BOOL`-returning API (which signals
/// success/failure, not a copied length) back to a `String`, cut at the
/// first NUL.
fn wide_buf_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `subst Z: C:\Windows` (or any directory) makes `QueryDosDeviceW`
    /// return `\??\C:\Windows` for `Z:` — a drive alias, not a device. This
    /// shape must never pass the guard `drive_mappings` relies on to decide
    /// what belongs in a `VolumeMap`, on any drive letter.
    #[test]
    fn subst_shaped_target_is_not_a_device_namespace_name() {
        assert!(!is_device_namespace_name(r"\??\C:\Windows"));
        assert!(!is_device_namespace_name(r"\??\Z:\SomeDir"));
        assert!(!is_device_namespace_name(r"\??\C:\Games"));
        // A real device name, including case-insensitively, still passes.
        assert!(is_device_namespace_name(r"\Device\HarddiskVolume3"));
        assert!(is_device_namespace_name(r"\device\harddiskvolume7"));
    }

    /// The current drive must show up with a `\Device\...`-shaped name.
    #[test]
    fn drive_mappings_includes_current_drive_with_device_name() {
        let cwd = std::env::current_dir().unwrap();
        let drive = cwd.to_string_lossy().chars().next().unwrap().to_ascii_uppercase();
        let mappings = drive_mappings();
        let mine = mappings.iter().find(|m| m.drive.eq_ignore_ascii_case(&drive));
        let mine = mine.unwrap_or_else(|| panic!("current drive {drive} not enumerated: {mappings:?}"));
        assert!(
            mine.device_name.starts_with(r"\Device\"),
            "unexpected device name: {}",
            mine.device_name
        );
    }

    /// `expand_long_path` round-trips a short name this test constructs.
    #[test]
    fn expand_long_path_round_trips_a_short_name() {
        let dir = std::env::temp_dir().join(format!("vfs-win-8dot3-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let long_path = dir.join("ThisIsALongFileNameForVfsWinTesting.txt");
        std::fs::write(&long_path, b"x").unwrap();
        let long_str = long_path.to_string_lossy().into_owned();

        if let Some(short) = short_path_name(&long_str) {
            if !short.eq_ignore_ascii_case(&long_str) {
                let expanded = expand_long_path(&short).expect("expansion should succeed");
                assert!(expanded.to_ascii_lowercase().contains("thisisalongfilenamefor"));
            }
            // else: 8.3 generation disabled on this volume, nothing to expand.
        }
        std::fs::remove_file(&long_path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    /// `final_path_for_open` succeeds on a real file and reports its name.
    #[test]
    fn final_path_for_open_resolves_an_existing_file() {
        let dir = std::env::temp_dir().join(format!("vfs-win-final-path-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("final-path.txt");
        std::fs::write(&path, b"x").unwrap();
        let path_str = path.to_string_lossy().into_owned();

        let resolved = final_path_for_open(&path_str).expect("should resolve an existing file");
        assert!(resolved.to_ascii_lowercase().contains("final-path.txt"));

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir(&dir).ok();
    }

    /// A path with nothing to open falls through to `None`, so callers know
    /// to try `expand_long_path` instead.
    #[test]
    fn final_path_for_open_is_none_for_a_nonexistent_path() {
        let missing = std::env::temp_dir().join("vfs-win-does-not-exist-xyz-12345.txt");
        assert!(final_path_for_open(&missing.to_string_lossy()).is_none());
    }

    /// `reparse_point_target` reads a real junction's substitute name
    /// without ever opening the target — proven directly by pointing the
    /// junction at a directory this test deletes *before* calling
    /// `reparse_point_target`. `final_path_for_open`/`GetFinalPathNameByHandleW`
    /// would fail outright once the target is gone (it has to open it); a
    /// function that reads the reparse point's own on-disk metadata must not
    /// care either way.
    #[test]
    fn reparse_point_target_reads_a_junction_without_opening_its_target() {
        let base =
            std::env::temp_dir().join(format!("vfs-win-reparse-target-test-{}", std::process::id()));
        let target = base.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let link = base.join("link");
        let _ = std::fs::remove_dir(&link);
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J", &link.to_string_lossy(), &target.to_string_lossy()])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !made {
            // No `mklink /J` support/privilege on this box — inconclusive.
            std::fs::remove_dir_all(&base).ok();
            return;
        }

        // Delete the target *after* creating the junction but *before*
        // reading it — an ordinary open-and-follow approach would now fail;
        // reading the reparse point's own metadata must not.
        std::fs::remove_dir(&target).ok();

        let got = reparse_point_target(&link.to_string_lossy())
            .expect("reparse_point_target should read the substitute name regardless of whether the target still exists");
        assert!(
            got.to_ascii_lowercase().contains("target"),
            "unexpected reparse target: {got}"
        );

        std::fs::remove_dir(&link).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    /// An ordinary directory (no reparse point) has nothing to report.
    #[test]
    fn reparse_point_target_is_none_for_an_ordinary_directory() {
        let dir =
            std::env::temp_dir().join(format!("vfs-win-reparse-target-ordinary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(reparse_point_target(&dir.to_string_lossy()).is_none());
        std::fs::remove_dir(&dir).ok();
    }

    /// A nonexistent path has nothing to report either — the failure mode
    /// must be `None`, not a panic.
    #[test]
    fn reparse_point_target_is_none_for_a_nonexistent_path() {
        let missing = std::env::temp_dir().join("vfs-win-reparse-target-missing-xyz-12345");
        assert!(reparse_point_target(&missing.to_string_lossy()).is_none());
    }
}
