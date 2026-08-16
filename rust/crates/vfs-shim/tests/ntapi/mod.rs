//! Minimal ntdll bindings for tests that need to name a file the way NT does.
//!
//! Win32 decides on its own whether a relative path becomes an absolute name or
//! a (directory handle + relative name) pair, so a test that only goes through
//! `std::fs` cannot guarantee it exercised the second form. These call the NT
//! layer directly so the handle-relative shape is certain to be covered.
#![allow(dead_code)]

use std::ffi::c_void;

pub const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
pub const OBJ_CASE_INSENSITIVE: u32 = 0x40;

#[repr(C)]
pub struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *mut u16,
}

#[repr(C)]
pub struct ObjectAttributes {
    pub length: u32,
    pub root_directory: *mut c_void,
    pub object_name: *const UnicodeString,
    pub attributes: u32,
    pub security_descriptor: *mut c_void,
    pub security_qos: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
pub struct FileBasicInformation {
    pub creation_time: i64,
    pub last_access_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
    pub file_attributes: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct FileNetworkOpenInformation {
    pub creation_time: i64,
    pub last_access_time: i64,
    pub last_write_time: i64,
    pub change_time: i64,
    pub allocation_size: i64,
    pub end_of_file: i64,
    pub file_attributes: u32,
}

type NtCreateFileFn = unsafe extern "system" fn(
    *mut *mut c_void,
    u32,
    *const ObjectAttributes,
    *mut c_void,
    *const i64,
    u32,
    u32,
    u32,
    u32,
    *const c_void,
    u32,
) -> i32;
type NtOpenFileFn = unsafe extern "system" fn(
    *mut *mut c_void,
    u32,
    *const ObjectAttributes,
    *mut c_void,
    u32,
    u32,
) -> i32;
type NtQueryAttributesFileFn =
    unsafe extern "system" fn(*const ObjectAttributes, *mut FileBasicInformation) -> i32;
type NtQueryFullAttributesFileFn =
    unsafe extern "system" fn(*const ObjectAttributes, *mut FileNetworkOpenInformation) -> i32;
type NtQueryInformationByNameFn = unsafe extern "system" fn(
    *const ObjectAttributes,
    *mut c_void,
    *mut c_void,
    u32,
    u32,
) -> i32;
/// `NtDeleteFile` takes an `OBJECT_ATTRIBUTES` and nothing else — no handle, no
/// access mask, no disposition. There is no Win32 wrapper that reaches it, so a
/// test that wants to exercise the path-based delete has to call it directly.
type NtDeleteFileFn = unsafe extern "system" fn(*const ObjectAttributes) -> i32;
type NtSetInformationFileFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void, u32, u32) -> i32;

fn ntdll_proc(name: &str) -> Option<*const c_void> {
    use windows_sys::Win32::Foundation::HMODULE;
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    let mut n: Vec<u8> = b"ntdll.dll\0".to_vec();
    let m: HMODULE = unsafe { GetModuleHandleA(n.as_mut_ptr()) };
    if m.is_null() {
        return None;
    }
    let mut fname: Vec<u8> = name.as_bytes().to_vec();
    fname.push(0);
    unsafe { GetProcAddress(m, fname.as_ptr()).map(|p| p as *const c_void) }
}

/// Builds an `OBJECT_ATTRIBUTES` naming `rel` beneath the directory `dir`.
pub struct RelName {
    _wide: Vec<u16>,
    _us: Box<UnicodeString>,
    pub oa: ObjectAttributes,
}

pub fn rel_name(dir: *mut c_void, rel: &str) -> RelName {
    let mut wide: Vec<u16> = rel.encode_utf16().collect();
    let bytes = (wide.len() * 2) as u16;
    let us = Box::new(UnicodeString {
        length: bytes,
        maximum_length: bytes,
        buffer: wide.as_mut_ptr(),
    });
    let oa = ObjectAttributes {
        length: core::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: dir,
        object_name: &*us as *const UnicodeString,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: core::ptr::null_mut(),
        security_qos: core::ptr::null_mut(),
    };
    RelName { _wide: wide, _us: us, oa }
}

/// Builds an `OBJECT_ATTRIBUTES` naming an absolute path with a **null**
/// `RootDirectory` — the shape `NtDeleteFile` is reached with in practice, and
/// the one that leaves the path itself as the only thing the call can be
/// decided on.
pub struct AbsName {
    _wide: Vec<u16>,
    _us: Box<UnicodeString>,
    pub oa: ObjectAttributes,
}

/// `\??\`-prefix a Win32 path; pass an NT path through unchanged.
pub fn to_nt(path: &str) -> String {
    if path.starts_with(r"\??\") || path.starts_with(r"\Device\") {
        path.to_string()
    } else {
        format!(r"\??\{path}")
    }
}

pub fn abs_name(path: &str) -> AbsName {
    let mut wide: Vec<u16> = to_nt(path).encode_utf16().collect();
    let bytes = (wide.len() * 2) as u16;
    let us = Box::new(UnicodeString {
        length: bytes,
        maximum_length: bytes,
        buffer: wide.as_mut_ptr(),
    });
    let oa = ObjectAttributes {
        length: core::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: core::ptr::null_mut(),
        object_name: &*us as *const UnicodeString,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: core::ptr::null_mut(),
        security_qos: core::ptr::null_mut(),
    };
    AbsName { _wide: wide, _us: us, oa }
}

/// `NtDeleteFile` against an absolute path. Returns the raw `NTSTATUS`.
pub fn nt_delete_file(path: &str) -> i32 {
    let Some(p) = ntdll_proc("NtDeleteFile") else { return -1 };
    let f: NtDeleteFileFn = unsafe { core::mem::transmute(p) };
    let n = abs_name(path);
    unsafe { f(&n.oa) }
}

pub const DELETE: u32 = 0x0001_0000;
pub const FILE_RENAME_INFORMATION: u32 = 10;
pub const FILE_RENAME_INFORMATION_EX: u32 = 65;

/// Open an absolute path with an explicit access mask (`DELETE` for the rename
/// below, which is what `MoveFileExW` itself asks for).
pub fn nt_open_abs(path: &str, access: u32) -> (i32, *mut c_void) {
    let Some(p) = ntdll_proc("NtOpenFile") else { return (-1, core::ptr::null_mut()) };
    let f: NtOpenFileFn = unsafe { core::mem::transmute(p) };
    let n = abs_name(path);
    let mut h: *mut c_void = core::ptr::null_mut();
    let mut iosb = [0u8; 16];
    let st = unsafe {
        f(
            &mut h,
            access | SYNCHRONIZE,
            &n.oa,
            iosb.as_mut_ptr() as *mut c_void,
            FILE_SHARE_ALL,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE,
        )
    };
    (st, h)
}

/// `NtSetInformationFile` with a `FILE_RENAME_INFORMATION`(`_EX`) naming an
/// absolute target (`RootDirectory` NULL), which is what `MoveFileExW` builds.
///
/// Layout, and it must match `hook.rs::parse_rename_target` exactly:
/// `ReplaceIfExists`/`Flags` at 0, `RootDirectory` at 8, `FileNameLength` at
/// 16, `FileName` at 20.
pub fn nt_rename(h: *mut c_void, target: &str, class: u32) -> i32 {
    let Some(p) = ntdll_proc("NtSetInformationFile") else { return -1 };
    let f: NtSetInformationFileFn = unsafe { core::mem::transmute(p) };
    let wide: Vec<u16> = to_nt(target).encode_utf16().collect();
    let namelen = wide.len() * 2;
    let mut buf = vec![0u8; 20 + namelen];
    buf[0] = 1; // ReplaceIfExists / FILE_RENAME_REPLACE_IF_EXISTS
    buf[16..20].copy_from_slice(&(namelen as u32).to_le_bytes());
    for (i, u) in wide.iter().enumerate() {
        buf[20 + i * 2..22 + i * 2].copy_from_slice(&u.to_le_bytes());
    }
    let mut iosb = [0u8; 16];
    unsafe {
        f(
            h,
            iosb.as_mut_ptr() as *mut c_void,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            class,
        )
    }
}

const GENERIC_READ: u32 = 0x8000_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const FILE_SHARE_ALL: u32 = 7;
const FILE_OPEN: u32 = 1;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;

/// `(status, handle)` from `NtCreateFile` against a directory handle.
pub fn nt_create_relative(dir: *mut c_void, rel: &str) -> (i32, *mut c_void) {
    let Some(p) = ntdll_proc("NtCreateFile") else { return (-1, core::ptr::null_mut()) };
    let f: NtCreateFileFn = unsafe { core::mem::transmute(p) };
    let n = rel_name(dir, rel);
    let mut h: *mut c_void = core::ptr::null_mut();
    let mut iosb = [0u8; 16];
    let st = unsafe {
        f(
            &mut h,
            GENERIC_READ | SYNCHRONIZE,
            &n.oa,
            iosb.as_mut_ptr() as *mut c_void,
            core::ptr::null(),
            0,
            FILE_SHARE_ALL,
            FILE_OPEN,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE,
            core::ptr::null(),
            0,
        )
    };
    (st, h)
}

pub fn nt_open_relative(dir: *mut c_void, rel: &str) -> (i32, *mut c_void) {
    let Some(p) = ntdll_proc("NtOpenFile") else { return (-1, core::ptr::null_mut()) };
    let f: NtOpenFileFn = unsafe { core::mem::transmute(p) };
    let n = rel_name(dir, rel);
    let mut h: *mut c_void = core::ptr::null_mut();
    let mut iosb = [0u8; 16];
    let st = unsafe {
        f(
            &mut h,
            GENERIC_READ | SYNCHRONIZE,
            &n.oa,
            iosb.as_mut_ptr() as *mut c_void,
            FILE_SHARE_ALL,
            FILE_SYNCHRONOUS_IO_NONALERT | FILE_NON_DIRECTORY_FILE,
        )
    };
    (st, h)
}

/// `(status, FileAttributes)`.
pub fn nt_query_attributes_relative(dir: *mut c_void, rel: &str) -> (i32, u32) {
    let Some(p) = ntdll_proc("NtQueryAttributesFile") else { return (-1, 0) };
    let f: NtQueryAttributesFileFn = unsafe { core::mem::transmute(p) };
    let n = rel_name(dir, rel);
    let mut info = FileBasicInformation::default();
    let st = unsafe { f(&n.oa, &mut info) };
    (st, info.file_attributes)
}

/// `(status, EndOfFile)`.
pub fn nt_query_full_attributes_relative(dir: *mut c_void, rel: &str) -> (i32, i64) {
    let Some(p) = ntdll_proc("NtQueryFullAttributesFile") else { return (-1, 0) };
    let f: NtQueryFullAttributesFileFn = unsafe { core::mem::transmute(p) };
    let n = rel_name(dir, rel);
    let mut info = FileNetworkOpenInformation::default();
    let st = unsafe { f(&n.oa, &mut info) };
    (st, info.end_of_file)
}

/// `(status, EndOfFile)` for the by-name stat classes, or `None` when the export
/// is absent (pre-1709 Windows).
pub fn nt_query_by_name_relative(dir: *mut c_void, rel: &str, class: u32) -> Option<(i32, i64)> {
    let p = ntdll_proc("NtQueryInformationByName")?;
    let f: NtQueryInformationByNameFn = unsafe { core::mem::transmute(p) };
    let n = rel_name(dir, rel);
    let mut buf = [0u8; 128];
    let mut iosb = [0u8; 16];
    let st = unsafe {
        f(
            &n.oa,
            iosb.as_mut_ptr() as *mut c_void,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            class,
        )
    };
    // EndOfFile sits at 48 for classes 68/77, at 40 for 34, at 8 for 5.
    let off = match class {
        68 | 77 => 48,
        34 => 40,
        5 => 8,
        _ => return Some((st, 0)),
    };
    Some((st, i64::from_le_bytes(buf[off..off + 8].try_into().unwrap())))
}

pub fn read_all(h: *mut c_void) -> Vec<u8> {
    // Read through the handle with NtReadFile so the test stays on the NT
    // surface it is exercising.
    type NtReadFileFn = unsafe extern "system" fn(
        *mut c_void, *mut c_void, *const c_void, *const c_void,
        *mut c_void, *mut c_void, u32, *const i64, *const u32,
    ) -> i32;
    let p = ntdll_proc("NtReadFile").expect("NtReadFile");
    let f: NtReadFileFn = unsafe { core::mem::transmute(p) };
    let mut out = vec![0u8; 4096];
    let mut iosb = [0u8; 16];
    let offset: i64 = 0;
    let st = unsafe {
        f(
            h,
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null(),
            iosb.as_mut_ptr() as *mut c_void,
            out.as_mut_ptr() as *mut c_void,
            out.len() as u32,
            &offset,
            core::ptr::null(),
        )
    };
    assert!(st >= 0, "NtReadFile failed: {st:#x}");
    let got = usize::from_le_bytes(iosb[8..16].try_into().unwrap());
    out.truncate(got);
    out
}

pub fn close(h: *mut c_void) {
    use windows_sys::Win32::Foundation::CloseHandle;
    if !h.is_null() {
        unsafe { CloseHandle(h as _) };
    }
}

type NtQueryDirectoryFileFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *const c_void,
    *const c_void,
    *mut c_void,
    *mut c_void,
    u32,
    u32,
    u8,
    *const UnicodeString,
    u8,
) -> i32;

/// Enumerates `dir` through the **classic** `NtQueryDirectoryFile`.
///
/// ntdll exports two enumeration entry points and a caller may use either. They
/// must return the same view, so this exists to be compared against the `Ex`
/// form that `std::fs::read_dir` uses.
pub fn nt_enum_classic(dir: *mut c_void) -> Vec<String> {
    const FILE_DIRECTORY_INFORMATION: u32 = 1;
    let Some(p) = ntdll_proc("NtQueryDirectoryFile") else { return Vec::new() };
    let f: NtQueryDirectoryFileFn = unsafe { core::mem::transmute(p) };
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut restart = 1u8;
    loop {
        let mut iosb = [0u8; 16];
        let st = unsafe {
            f(
                dir,
                core::ptr::null_mut(),
                core::ptr::null(),
                core::ptr::null(),
                iosb.as_mut_ptr() as *mut c_void,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                FILE_DIRECTORY_INFORMATION,
                0,
                core::ptr::null(),
                restart,
            )
        };
        restart = 0;
        if st < 0 {
            break; // STATUS_NO_MORE_FILES ends the walk
        }
        // FILE_DIRECTORY_INFORMATION: NextEntryOffset@0, FileNameLength@60,
        // FileName@64.
        let mut off = 0usize;
        loop {
            let next = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
            let namelen =
                u32::from_le_bytes(buf[off + 60..off + 64].try_into().unwrap()) as usize;
            let start = off + 64;
            if start + namelen <= buf.len() {
                let units: Vec<u16> = buf[start..start + namelen]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                out.push(String::from_utf16_lossy(&units));
            }
            if next == 0 {
                break;
            }
            off += next;
        }
    }
    out
}
