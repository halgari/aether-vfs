//! The ntdll detours. ALL `unsafe` in the crate lives here.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use retour::RawDetour;
use vfs_redirect::{
    parse_full_dir_info, write_dir_info, AttrDecision, Decision, DirInfoClass, DirItem, DirStatus,
};
use windows_sys::Win32::Foundation::{HANDLE, HMODULE, NTSTATUS};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

use crate::engine::Engine;
use crate::ntdef::{
    FileBasicInformation, FileNetworkOpenInformation, NtCloseFn, NtCreateFileFn, NtOpenFileFn,
    NtQueryAttributesFileFn, NtQueryDirectoryFileExFn, NtQueryFullAttributesFileFn,
    ObjectAttributes, UnicodeString, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    SL_RESTART_SCAN, SL_RETURN_SINGLE_ENTRY, STATUS_BUFFER_OVERFLOW, STATUS_NO_MORE_FILES,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
};

/// Errors installing the hooks.
#[derive(Debug)]
pub enum InstallError {
    AlreadyInstalled,
    NtdllMissing,
    ProcMissing,
    Detour,
}

static ENGINE: OnceLock<Engine> = OnceLock::new();
// Each set once, before any detour is enabled; only read from the hooks after.
static mut TRAMP_CREATE: Option<NtCreateFileFn> = None;
static mut TRAMP_QATTR: Option<NtQueryAttributesFileFn> = None;
static mut TRAMP_QFULL: Option<NtQueryFullAttributesFileFn> = None;
static mut TRAMP_OPEN: Option<NtOpenFileFn> = None;
static mut TRAMP_QDIREX: Option<NtQueryDirectoryFileExFn> = None;
static mut TRAMP_CLOSE: Option<NtCloseFn> = None;

/// Per-handle enumeration cursor over a merged directory listing.
struct EnumState {
    merged: Vec<DirItem>,
    cursor: usize,
}

/// A tracked directory handle: the NT path it was opened as, and its lazily
/// built enumeration state (rebuilt on `SL_RESTART_SCAN`).
struct DirTracked {
    dir_nt_path: String,
    state: Option<EnumState>,
}

/// Handle value (`isize`) -> tracking. `BTreeMap::new()` is `const`, so this
/// needs no lazy init. Populated by the `NtCreateFile` hook, drained by
/// `NtClose`.
static DIR_TABLE: Mutex<BTreeMap<isize, DirTracked>> = Mutex::new(BTreeMap::new());

/// Keeps the detours alive; dropping it disables the hooks.
pub struct HookGuard {
    _detours: Vec<RawDetour>,
}

/// Resolve `name` in ntdll and build (not yet enabled) a detour to `hookfn`.
unsafe fn make_detour(
    ntdll: HMODULE,
    name: &[u8],
    hookfn: *const (),
) -> Result<RawDetour, InstallError> {
    let proc = GetProcAddress(ntdll, name.as_ptr()).ok_or(InstallError::ProcMissing)?;
    RawDetour::new(proc as *const (), hookfn).map_err(|_| InstallError::Detour)
}

/// Install all read-path detours backed by `engine`. Idempotent-guarded.
pub fn install(engine: Engine) -> Result<HookGuard, InstallError> {
    ENGINE.set(engine).map_err(|_| InstallError::AlreadyInstalled)?;

    // SAFETY: standard ntdll lookup + detour install; each hook matches its
    // function's ABI; each trampoline is stored before any detour is enabled.
    unsafe {
        let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
        if ntdll.is_null() {
            return Err(InstallError::NtdllMissing);
        }

        let d_create = make_detour(ntdll, b"NtCreateFile\0", create_hook as *const ())?;
        TRAMP_CREATE = Some(core::mem::transmute::<*const (), NtCreateFileFn>(
            d_create.trampoline() as *const (),
        ));
        let d_qattr = make_detour(ntdll, b"NtQueryAttributesFile\0", qattr_hook as *const ())?;
        TRAMP_QATTR = Some(core::mem::transmute::<*const (), NtQueryAttributesFileFn>(
            d_qattr.trampoline() as *const (),
        ));
        let d_qfull =
            make_detour(ntdll, b"NtQueryFullAttributesFile\0", qfull_hook as *const ())?;
        TRAMP_QFULL = Some(core::mem::transmute::<*const (), NtQueryFullAttributesFileFn>(
            d_qfull.trampoline() as *const (),
        ));
        let d_open = make_detour(ntdll, b"NtOpenFile\0", open_hook as *const ())?;
        TRAMP_OPEN = Some(core::mem::transmute::<*const (), NtOpenFileFn>(
            d_open.trampoline() as *const (),
        ));
        let d_qdirex =
            make_detour(ntdll, b"NtQueryDirectoryFileEx\0", qdirex_hook as *const ())?;
        TRAMP_QDIREX = Some(core::mem::transmute::<*const (), NtQueryDirectoryFileExFn>(
            d_qdirex.trampoline() as *const (),
        ));
        let d_close = make_detour(ntdll, b"NtClose\0", close_hook as *const ())?;
        TRAMP_CLOSE = Some(core::mem::transmute::<*const (), NtCloseFn>(
            d_close.trampoline() as *const (),
        ));

        d_create.enable().map_err(|_| InstallError::Detour)?;
        d_qattr.enable().map_err(|_| InstallError::Detour)?;
        d_qfull.enable().map_err(|_| InstallError::Detour)?;
        d_open.enable().map_err(|_| InstallError::Detour)?;
        d_qdirex.enable().map_err(|_| InstallError::Detour)?;
        d_close.enable().map_err(|_| InstallError::Detour)?;

        Ok(HookGuard { _detours: vec![d_create, d_qattr, d_qfull, d_open, d_qdirex, d_close] })
    }
}

/// Decode a fully-qualified ObjectName. `None` when ineligible (null/relative OA
/// or empty name).
unsafe fn path_of(oa: *const ObjectAttributes) -> Option<String> {
    if oa.is_null() {
        return None;
    }
    let oa_ref = &*oa;
    if !oa_ref.root_directory.is_null() || oa_ref.object_name.is_null() {
        return None;
    }
    let us = &*oa_ref.object_name;
    if us.buffer.is_null() {
        return None;
    }
    let units = core::slice::from_raw_parts(us.buffer, us.length as usize / 2);
    Some(String::from_utf16_lossy(units))
}

/// Decode + ask the engine what to do with an open.
unsafe fn decision_for(oa: *const ObjectAttributes) -> Option<Decision> {
    let engine = ENGINE.get()?;
    let path = path_of(oa)?;
    Some(engine.decide(&path))
}

/// Record a freshly-opened handle as a candidate directory for enumeration
/// virtualization: only when the open succeeded and its path is under the
/// managed root. Harmless for file handles (they never receive a dir-enum call)
/// and reclaimed by `NtClose`. Shared by the `NtCreateFile` and `NtOpenFile`
/// pass-through paths.
unsafe fn tag_under_root(
    file_handle: *mut HANDLE,
    oa: *const ObjectAttributes,
    status: NTSTATUS,
) {
    // NT_SUCCESS is status >= 0.
    if status < 0 || file_handle.is_null() {
        return;
    }
    if let Some(engine) = ENGINE.get() {
        if let Some(path) = path_of(oa) {
            if engine.is_under_root(&path) {
                if let Ok(mut table) = DIR_TABLE.lock() {
                    table.insert(
                        *file_handle as isize,
                        DirTracked { dir_nt_path: path, state: None },
                    );
                }
            }
        }
    }
}

unsafe extern "system" fn create_hook(
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
    let tramp = match TRAMP_CREATE {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    match decision_for(oa) {
        Some(Decision::Redirect { target_nt }) => {
            let mut wbuf: Vec<u16> = target_nt.encode_utf16().collect();
            let byte_len = (wbuf.len() * 2) as u16;
            let new_us = UnicodeString {
                length: byte_len,
                maximum_length: byte_len,
                buffer: wbuf.as_mut_ptr(),
            };
            let oa_ref = &*oa;
            let new_oa = ObjectAttributes {
                length: oa_ref.length,
                root_directory: core::ptr::null_mut(),
                object_name: &new_us,
                attributes: oa_ref.attributes,
                security_descriptor: oa_ref.security_descriptor,
                security_qos: oa_ref.security_qos,
            };
            let status = tramp(
                file_handle, access, &new_oa, iosb, alloc, attrs, share, disp, opts, ea, ealen,
            );
            drop(wbuf);
            status
        }
        Some(Decision::Deny) => STATUS_OBJECT_NAME_NOT_FOUND,
        Some(Decision::PassThrough) | None => {
            let status =
                tramp(file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen);
            tag_under_root(file_handle, oa, status);
            status
        }
    }
}

/// `NtOpenFile` hook. Mirrors `create_hook` (redirect / deny / pass-through +
/// dir tagging) for the open path that Rust `std` and many Win32 callers use to
/// open existing files and directories.
unsafe extern "system" fn open_hook(
    file_handle: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    share: u32,
    opts: u32,
) -> NTSTATUS {
    let tramp = match TRAMP_OPEN {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    match decision_for(oa) {
        Some(Decision::Redirect { target_nt }) => {
            let mut wbuf: Vec<u16> = target_nt.encode_utf16().collect();
            let byte_len = (wbuf.len() * 2) as u16;
            let new_us = UnicodeString {
                length: byte_len,
                maximum_length: byte_len,
                buffer: wbuf.as_mut_ptr(),
            };
            let oa_ref = &*oa;
            let new_oa = ObjectAttributes {
                length: oa_ref.length,
                root_directory: core::ptr::null_mut(),
                object_name: &new_us,
                attributes: oa_ref.attributes,
                security_descriptor: oa_ref.security_descriptor,
                security_qos: oa_ref.security_qos,
            };
            let status = tramp(file_handle, access, &new_oa, iosb, share, opts);
            drop(wbuf);
            status
        }
        Some(Decision::Deny) => STATUS_OBJECT_NAME_NOT_FOUND,
        Some(Decision::PassThrough) | None => {
            let status = tramp(file_handle, access, oa, iosb, share, opts);
            tag_under_root(file_handle, oa, status);
            status
        }
    }
}

unsafe extern "system" fn qattr_hook(
    oa: *const ObjectAttributes,
    info: *mut FileBasicInformation,
) -> NTSTATUS {
    let tramp = match TRAMP_QATTR {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if let Some(engine) = ENGINE.get() {
        if let Some(path) = path_of(oa) {
            match engine.query_attributes(&path) {
                AttrDecision::Attributes { is_dir, .. } => {
                    if !info.is_null() {
                        (*info).creation_time = 0;
                        (*info).last_access_time = 0;
                        (*info).last_write_time = 0;
                        (*info).change_time = 0;
                        (*info).file_attributes =
                            if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
                    }
                    return STATUS_SUCCESS;
                }
                AttrDecision::Deny => return STATUS_OBJECT_NAME_NOT_FOUND,
                AttrDecision::PassThrough => {}
            }
        }
    }
    tramp(oa, info)
}

unsafe extern "system" fn qfull_hook(
    oa: *const ObjectAttributes,
    info: *mut FileNetworkOpenInformation,
) -> NTSTATUS {
    let tramp = match TRAMP_QFULL {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if let Some(engine) = ENGINE.get() {
        if let Some(path) = path_of(oa) {
            match engine.query_attributes(&path) {
                AttrDecision::Attributes { is_dir, size, .. } => {
                    if !info.is_null() {
                        (*info).creation_time = 0;
                        (*info).last_access_time = 0;
                        (*info).last_write_time = 0;
                        (*info).change_time = 0;
                        (*info).allocation_size = size as i64;
                        (*info).end_of_file = size as i64;
                        (*info).file_attributes =
                            if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
                    }
                    return STATUS_SUCCESS;
                }
                AttrDecision::Deny => return STATUS_OBJECT_NAME_NOT_FOUND,
                AttrDecision::PassThrough => {}
            }
        }
    }
    tramp(oa, info)
}

/// Reclaim any tracking for a closing handle before the OS (possibly) reuses
/// its value.
unsafe extern "system" fn close_hook(handle: HANDLE) -> NTSTATUS {
    let tramp = match TRAMP_CLOSE {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if let Ok(mut table) = DIR_TABLE.lock() {
        table.remove(&(handle as isize));
    }
    tramp(handle)
}

/// Extract a search wildcard from a `PUNICODE_STRING`. Null/empty/`*`/`*.*`
/// mean "match everything" (`None`).
unsafe fn wildcard_of(file_name: *const UnicodeString) -> Option<String> {
    if file_name.is_null() {
        return None;
    }
    let us = &*file_name;
    if us.buffer.is_null() || us.length == 0 {
        return None;
    }
    let units = core::slice::from_raw_parts(us.buffer, us.length as usize / 2);
    let s = String::from_utf16_lossy(units);
    if s.is_empty() || s == "*" || s == "*.*" {
        None
    } else {
        Some(s)
    }
}

/// Drain a real directory's entries by calling the trampoline in class 2
/// (FileFullDirectoryInformation) with SL_RESTART_SCAN, until a negative status
/// (STATUS_NO_MORE_FILES or any error). The trampoline bypasses this detour, so
/// draining does not recurse.
unsafe fn drain_real(handle: HANDLE, tramp: NtQueryDirectoryFileExFn) -> Vec<DirItem> {
    const CLASS_FULL_DIR: u32 = 2;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut first = true;
    loop {
        let mut local_iosb = [0u8; 16];
        let flags = if first { SL_RESTART_SCAN } else { 0 };
        first = false;
        let st = tramp(
            handle,
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null(),
            local_iosb.as_mut_ptr() as *mut c_void,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            CLASS_FULL_DIR,
            flags,
            core::ptr::null(),
        );
        if st < 0 {
            break; // STATUS_NO_MORE_FILES or an error ends the drain
        }
        out.extend(parse_full_dir_info(&buf));
    }
    out
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn qdirex_hook(
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
    let tramp = match TRAMP_QDIREX {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    let passthrough =
        || tramp(handle, event, apc, apc_ctx, iosb, info, length, class_raw, flags, file_name);

    // Unknown info class -> let the OS handle it verbatim.
    let class = match DirInfoClass::from_u32(class_raw) {
        Some(c) => c,
        None => return passthrough(),
    };
    let key = handle as isize;
    let restart = flags & SL_RESTART_SCAN != 0;
    let single = flags & SL_RETURN_SINGLE_ENTRY != 0;

    // Phase 1 (locked): is this a tracked handle, and must we (re)build?
    let (need_build, dir_path) = {
        let table = match DIR_TABLE.lock() {
            Ok(t) => t,
            Err(_) => return passthrough(),
        };
        match table.get(&key) {
            None => return passthrough(),
            Some(t) => (restart || t.state.is_none(), t.dir_nt_path.clone()),
        }
    };

    // Phase 2 (unlocked): drain the real dir + merge. `drain_real` calls the
    // syscall, so the lock must NOT be held here (NtClose also takes it).
    let rebuilt = if need_build {
        let wildcard = wildcard_of(file_name);
        let real = drain_real(handle, tramp);
        Some(match ENGINE.get() {
            Some(engine) => engine.merge_directory(&dir_path, &real, wildcard.as_deref()),
            None => real,
        })
    } else {
        None
    };

    // Phase 3 (locked): store the merged view (if rebuilt) and serve a slice.
    let mut table = match DIR_TABLE.lock() {
        Ok(t) => t,
        Err(_) => return passthrough(),
    };
    let tracked = match table.get_mut(&key) {
        Some(t) => t,
        None => return passthrough(),
    };
    if let Some(merged) = rebuilt {
        tracked.state = Some(EnumState { merged, cursor: 0 });
    }
    let st = match tracked.state.as_mut() {
        Some(s) => s,
        None => return passthrough(),
    };
    let buf = core::slice::from_raw_parts_mut(info as *mut u8, length as usize);
    let result = write_dir_info(class, &st.merged[st.cursor..], buf, single);
    st.cursor += result.count;
    drop(table);

    let status = match result.status {
        DirStatus::Success => STATUS_SUCCESS,
        DirStatus::NoMoreFiles => STATUS_NO_MORE_FILES,
        DirStatus::BufferOverflow => STATUS_BUFFER_OVERFLOW,
    };
    // IO_STATUS_BLOCK: Status (NTSTATUS) @0, Information (ULONG_PTR) @8.
    if !iosb.is_null() {
        let p = iosb as *mut u8;
        core::ptr::write_unaligned(p as *mut u32, status as u32);
        core::ptr::write_unaligned(p.add(8) as *mut usize, result.bytes);
    }
    status
}
