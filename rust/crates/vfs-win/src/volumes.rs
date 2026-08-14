//! Raw Win32 lookups behind path canonicalisation: which drive letters exist
//! right now, what NT device name and volume-GUID mount point each currently
//! resolves to, and expanding a short (8.3) name — or any other alternate
//! spelling of the same file (junction, hardlink, `subst`/mapped drive) — to
//! its real path. All Win32 FFI for this is confined here; every caller gets
//! back plain, owned `String`s.
#![allow(unsafe_code)]

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFinalPathNameByHandleW, GetLogicalDrives, GetLongPathNameW,
    GetShortPathNameW, GetVolumeNameForVolumeMountPointW, QueryDosDeviceW,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};

/// What one currently-mounted drive letter resolves to at the NT layer.
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

/// Every drive letter Windows currently reports as in use, with each one's
/// device name and volume-GUID mount point. Cheap enough to call once per
/// session, but the result is a snapshot: a drive mounted or unmounted
/// afterwards is not reflected until this is called again.
pub fn drive_mappings() -> Vec<DriveMapping> {
    let mask = logical_drives_mask();
    (0..26u32)
        .filter(|bit| mask & (1 << bit) != 0)
        .filter_map(|bit| {
            let drive = (b'A' + bit as u8) as char;
            query_dos_device(drive).map(|device_name| DriveMapping {
                drive,
                device_name,
                volume_guid_win32: volume_guid_for_drive(drive),
            })
        })
        .collect()
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
/// query-only handle and ask `GetFinalPathNameByHandleW`, which resolves 8.3
/// short names, junctions, hardlinks, and `subst`/mapped drives all at once
/// — each is a different spelling of "what does this handle actually point
/// at", a question the OS already has to answer to service the open.
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
    let result = grow_to_fit(|buf| {
        // SAFETY: FFI. `handle` was just opened successfully above and is
        // closed exactly once, immediately below, regardless of outcome;
        // `buf` is valid for `buf.len()` `u16`s, matching `cchfilepath`.
        unsafe {
            GetFinalPathNameByHandleW(handle, buf.as_mut_ptr(), buf.len() as u32, FILE_NAME_NORMALIZED)
        }
    });
    // SAFETY: FFI. `handle` is the same valid handle opened above, closed
    // exactly once here on every path out of this function.
    unsafe {
        CloseHandle(handle);
    }
    result
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
}
