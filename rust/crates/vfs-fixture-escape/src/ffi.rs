//! Minimal, dependency-free Win32/NT bindings for the escape fixture. No
//! crate dependency (no `windows-sys`) — everything the fixture needs is a
//! handful of `kernel32` exports (always linked on a Windows MSVC target)
//! plus `ntdll!NtCreateFile`, looked up dynamically via `GetProcAddress`
//! exactly the way `vfs-shim`'s own NT test helpers do, so no `ntdll.lib`
//! import library is needed at link time either.
#![allow(non_snake_case, non_camel_case_types)]

use std::ffi::c_void;

pub type Handle = *mut c_void;

pub const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

pub const GENERIC_READ: u32 = 0x8000_0000;
pub const FILE_SHARE_READ: u32 = 0x1;
pub const FILE_SHARE_WRITE: u32 = 0x2;
pub const FILE_SHARE_DELETE: u32 = 0x4;
pub const OPEN_EXISTING: u32 = 3;
pub const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

pub const ERROR_FILE_NOT_FOUND: u32 = 2;
pub const ERROR_PATH_NOT_FOUND: u32 = 3;

pub const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;

#[link(name = "kernel32")]
extern "system" {
    pub fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: u32,
        dwShareMode: u32,
        lpSecurityAttributes: *mut c_void,
        dwCreationDisposition: u32,
        dwFlagsAndAttributes: u32,
        hTemplateFile: Handle,
    ) -> Handle;

    pub fn CloseHandle(hObject: Handle) -> i32;
    pub fn GetLastError() -> u32;
    pub fn GetLogicalDrives() -> u32;

    pub fn ReadFile(
        hFile: Handle,
        lpBuffer: *mut c_void,
        nNumberOfBytesToRead: u32,
        lpNumberOfBytesRead: *mut u32,
        lpOverlapped: *mut c_void,
    ) -> i32;

    pub fn QueryDosDeviceW(lpDeviceName: *const u16, lpTargetPath: *mut u16, ucchMax: u32) -> u32;

    /// Name-based attribute query — no handle, no `CreateFileW`. Windows
    /// itself routes this to `NtQueryAttributesFile`/`NtQueryFullAttributesFile`
    /// or (Windows 11) `NtQueryInformationByName`'s `FileStatBasicInformation`
    /// class, exactly the hook family (`qattr_hook`/`qfull_hook`/`qibn_hook`,
    /// `vfs-shim/src/hook.rs`) the metadata-gap test (vector `4m`, below)
    /// exercises. Returns `INVALID_FILE_ATTRIBUTES` on failure; call
    /// `GetLastError` for why.
    pub fn GetFileAttributesW(lpFileName: *const u16) -> u32;

    pub fn GetVolumeNameForVolumeMountPointW(
        lpszVolumeMountPoint: *const u16,
        lpszVolumeName: *mut u16,
        cchBufferLength: u32,
    ) -> i32;

    pub fn GetShortPathNameW(lpszLongPath: *const u16, lpszShortPath: *mut u16, cchBuffer: u32) -> u32;

    pub fn GetModuleHandleW(lpModuleName: *const u16) -> Handle;
    pub fn GetProcAddress(hModule: Handle, lpProcName: *const u8) -> *mut c_void;

    pub fn CreatePipe(
        hReadPipe: *mut Handle,
        hWritePipe: *mut Handle,
        lpPipeAttributes: *mut c_void,
        nSize: u32,
    ) -> i32;
}

/// NT `UNICODE_STRING` layout.
#[repr(C)]
pub struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *mut u16,
}

/// NT `OBJECT_ATTRIBUTES` layout.
#[repr(C)]
pub struct ObjectAttributes {
    pub length: u32,
    pub root_directory: Handle,
    pub object_name: *const UnicodeString,
    pub attributes: u32,
    pub security_descriptor: *const c_void,
    pub security_qos: *const c_void,
}

pub const OBJ_CASE_INSENSITIVE: u32 = 0x40;
pub const FILE_SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
pub const FILE_OPEN: u32 = 1;
pub const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
pub const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
pub const SYNCHRONIZE: u32 = 0x0010_0000;

/// The NT device name a drive letter is currently a `\DosDevices` symbolic
/// link to (e.g. `C:` -> `\Device\HarddiskVolume3`), or `None` if
/// `QueryDosDeviceW` reports nothing for it. `QueryDosDeviceW` can return more
/// than one NUL-separated target for a drive with stacked mappings; the first
/// is the current one.
pub fn query_dos_device(drive: char) -> Option<String> {
    let device = wide(&format!("{drive}:"));
    let mut buf = vec![0u16; 4096];
    // SAFETY: FFI. `device` is a valid NUL-terminated UTF-16 pointer; `buf`
    // is valid for `buf.len()` `u16`s, matching `ucchmax`.
    let len = unsafe { QueryDosDeviceW(device.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    if len == 0 {
        return None;
    }
    let first = buf.split(|&c| c == 0).next().unwrap_or(&[]);
    if first.is_empty() {
        None
    } else {
        Some(String::from_utf16_lossy(first))
    }
}

/// The volume-GUID mount point a drive letter currently resolves to
/// (`\\?\Volume{guid}\`, trailing separator included), or `None` if the
/// drive has no such mount point.
pub fn volume_guid_for_drive(drive: char) -> Option<String> {
    let mount_point = wide(&format!("{drive}:\\"));
    let mut buf = vec![0u16; 130]; // MSDN: 50 is guaranteed sufficient; rounded up.
    // SAFETY: FFI. `mount_point` is a valid NUL-terminated UTF-16 pointer
    // ending in a separator as this API requires; `buf` is valid for
    // `buf.len()` `u16`s, matching `cchbufferlength`.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(mount_point.as_ptr(), buf.as_mut_ptr(), buf.len() as u32)
    };
    if ok == 0 {
        None
    } else {
        Some(wide_to_string(&buf))
    }
}

type NtCreateFileFn = unsafe extern "system" fn(
    *mut Handle,
    u32,
    *const ObjectAttributes,
    *mut c_void, // IoStatusBlock
    *const i64,
    u32,
    u32,
    u32,
    u32,
    *const c_void,
    u32,
) -> i32;

/// Encode a Rust `&str` as a NUL-terminated UTF-16 buffer for the `*W` APIs.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decode a fixed wide buffer written by a `BOOL`-returning API back to a
/// `String`, cut at the first NUL.
pub fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// Call `f` with a growing buffer until it reports success (shared
/// `QueryDosDeviceW` / `GetShortPathNameW` convention: `0` is failure, a
/// return `>=` the buffer length is the required size to retry with).
pub fn grow_to_fit(mut f: impl FnMut(&mut [u16]) -> u32) -> Option<String> {
    let mut cap = 260usize;
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

/// `ntdll!NtCreateFile`'s address, looked up dynamically so the fixture never
/// needs an `ntdll.lib` import library (Microsoft does not ship one for
/// direct user-mode linking). `None` only if `ntdll.dll` or the export is
/// somehow absent, which does not happen on any supported Windows version but
/// is handled rather than unwrapped regardless.
fn nt_create_file_proc() -> Option<NtCreateFileFn> {
    let module = unsafe { GetModuleHandleW(wide("ntdll.dll").as_ptr()) };
    if module.is_null() {
        return None;
    }
    let mut name = b"NtCreateFile\0".to_vec();
    let proc = unsafe { GetProcAddress(module, name.as_mut_ptr()) };
    if proc.is_null() {
        return None;
    }
    // SAFETY: `proc` is the address `GetProcAddress` returned for the
    // `NtCreateFile` export by name; its signature is the well-documented,
    // stable-since-NT4 `ntdll!NtCreateFile` ABI reproduced above.
    Some(unsafe { std::mem::transmute::<*mut c_void, NtCreateFileFn>(proc) })
}

/// Why an `NtCreateFile` attempt did not report `Ok`.
#[derive(Debug)]
pub enum NtCreateError {
    /// `ntdll!NtCreateFile` could not be resolved at all (never observed on
    /// any supported Windows version, but not assumed away).
    Unresolved,
    /// The raw `NTSTATUS` `NtCreateFile` returned.
    Status(i32),
}

/// Attempt `NtCreateFile` for `relative_name`, resolved against
/// `root_directory` as `OBJECT_ATTRIBUTES.RootDirectory` — the handle-relative
/// open shape a game can use instead of naming a file by its full path.
pub fn nt_create_relative(
    root_directory: Handle,
    relative_name: &str,
) -> Result<Handle, NtCreateError> {
    let Some(f) = nt_create_file_proc() else {
        return Err(NtCreateError::Unresolved);
    };
    let mut name_wide: Vec<u16> = relative_name.encode_utf16().collect();
    let name_bytes = (name_wide.len() * 2) as u16;
    let unicode_name = UnicodeString {
        length: name_bytes,
        maximum_length: name_bytes,
        buffer: name_wide.as_mut_ptr(),
    };
    let oa = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root_directory,
        object_name: &unicode_name,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: std::ptr::null(),
        security_qos: std::ptr::null(),
    };
    let mut handle: Handle = std::ptr::null_mut();
    let mut io_status_block = [0u8; 16];
    // SAFETY: FFI into `ntdll!NtCreateFile`. `oa` is a validly constructed
    // `OBJECT_ATTRIBUTES` whose `object_name` and the `UnicodeString`'s
    // `buffer` both outlive this call (locals on this stack frame, not
    // dropped until after `f` returns). `io_status_block` is a 16-byte
    // scratch buffer, large enough for the ABI's `IO_STATUS_BLOCK`.
    let status = unsafe {
        f(
            &mut handle,
            GENERIC_READ | SYNCHRONIZE,
            &oa,
            io_status_block.as_mut_ptr() as *mut c_void,
            std::ptr::null(),
            0,
            FILE_SHARE_ALL,
            FILE_OPEN,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE,
            std::ptr::null(),
            0,
        )
    };
    if status >= 0 {
        Ok(handle)
    } else {
        Err(NtCreateError::Status(status))
    }
}

/// Open `path` (an arbitrary, already-fully-formed Win32 or extended-length
/// path string — the caller decides which spelling) read-only, sharing
/// everything, never creating anything. Returns the raw handle on success;
/// the caller closes it.
pub fn create_file_read(path_wide: &[u16]) -> Result<Handle, u32> {
    // SAFETY: FFI. `path_wide` is a NUL-terminated UTF-16 buffer (built by
    // `wide`); no other pointer is dereferenced, and `hTemplateFile` is null
    // as the API allows for `OPEN_EXISTING`.
    let handle = unsafe {
        CreateFileW(
            path_wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_ALL,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        // SAFETY: FFI, no arguments.
        Err(unsafe { GetLastError() })
    } else {
        Ok(handle)
    }
}

/// Read the whole content of an already-open, synchronous, readable
/// `handle`, up to a bound generous enough for any target this fixture is
/// realistically pointed at (a game data file, not a multi-gigabyte
/// archive). `None` if the first `ReadFile` call itself fails (a directory
/// handle, an unreadable synthetic handle, ...) — distinct from an empty
/// file, which reads zero bytes successfully and returns `Some(vec![])`.
///
/// Used only for the byte-identity check the positive canary needs: this
/// fixture otherwise never cares what a successful open actually contains.
pub fn read_all(handle: Handle) -> Option<Vec<u8>> {
    const CHUNK: usize = 64 * 1024;
    const MAX_CHUNKS: usize = 16; // 1 MiB cap — ample for a fixture target.
    let mut out = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    for i in 0..MAX_CHUNKS {
        let mut read: u32 = 0;
        // SAFETY: FFI. `handle` is a valid, caller-owned open handle; `buf`
        // is valid for `CHUNK` bytes, matching `nNumberOfBytesToRead`;
        // `read` is a valid local `u32` out-pointer. `lpOverlapped` is null,
        // matching the synchronous handles every caller here opens.
        let ok = unsafe {
            ReadFile(handle, buf.as_mut_ptr() as *mut c_void, CHUNK as u32, &mut read, std::ptr::null_mut())
        };
        if ok == 0 {
            return if i == 0 { None } else { Some(out) };
        }
        out.extend_from_slice(&buf[..read as usize]);
        if (read as usize) < CHUNK {
            break;
        }
    }
    Some(out)
}

/// Close a handle opened by this module. No-op on null/invalid.
pub fn close(handle: Handle) {
    if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
        // SAFETY: FFI; `handle` is a valid open handle per the caller's
        // contract (every call site here got it from a successful open in
        // this same module and closes it at most once).
        unsafe {
            CloseHandle(handle);
        }
    }
}
