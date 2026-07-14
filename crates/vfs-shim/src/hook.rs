//! The ntdll detours. ALL `unsafe` in the crate lives here.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use retour::RawDetour;
use vfs_redirect::{
    nt_to_volume_relative, parse_full_dir_info, write_dir_info, write_file_name_info, AttrDecision,
    Decision, DirInfoClass, DirItem, DirStatus,
};
use windows_sys::Win32::Foundation::{HANDLE, HMODULE, NTSTATUS};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::Threading::{
    ResumeThread, CREATE_SUSPENDED, PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::engine::Engine;
use crate::inject::{inject_dll, self_dll_path, wait_ready};
use crate::ntdef::{
    FileBasicInformation, FileNetworkOpenInformation, NtCloseFn, NtCreateFileFn, NtOpenFileFn,
    NtQueryAttributesFileFn, NtQueryDirectoryFileExFn, NtQueryFullAttributesFileFn,
    NtQueryInformationFileFn, NtSetInformationFileFn, ObjectAttributes, UnicodeString,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_DISPOSITION_DELETE,
    FILE_DISPOSITION_INFORMATION, FILE_DISPOSITION_INFORMATION_EX, FILE_NORMALIZED_NAME_INFORMATION,
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
static mut TRAMP_QIF: Option<NtQueryInformationFileFn> = None;
static mut TRAMP_SETINFO: Option<NtSetInformationFileFn> = None;
static mut TRAMP_CPIW: Option<CreateProcessInternalWFn> = None;

/// `kernelbase!CreateProcessInternalW` — the funnel under all CreateProcess*.
/// 12 params; only `flags` and `pi` are inspected/modified by the hook.
type CreateProcessInternalWFn = unsafe extern "system" fn(
    HANDLE,        // hToken
    *const u16,    // lpApplicationName
    *mut u16,      // lpCommandLine
    *const c_void, // lpProcessAttributes
    *const c_void, // lpThreadAttributes
    i32,           // bInheritHandles
    u32,           // dwCreationFlags
    *const c_void, // lpEnvironment
    *const u16,    // lpCurrentDirectory
    *const STARTUPINFOW,
    *mut PROCESS_INFORMATION,
    *mut HANDLE, // phNewToken
) -> i32;

/// This shim's own DLL path on disk, resolved once at install so the
/// process-creation hook can inject the same DLL into children.
static SELF_DLL: OnceLock<String> = OnceLock::new();

/// How long a spawning process waits for a child's shim to install its hooks
/// before resuming the child anyway (unvirtualized rather than hung).
const CHILD_READY_TIMEOUT_MS: u32 = 5_000;

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

/// Redirected-file handle -> virtual volume-relative path (identity spoof).
static IDENTITY_TABLE: Mutex<BTreeMap<isize, String>> = Mutex::new(BTreeMap::new());

/// Any under-root open's handle -> folded vpath components, so a later
/// handle-based delete/rename (NtSetInformationFile) can act by vpath.
static PATH_TABLE: Mutex<BTreeMap<isize, Vec<String>>> = Mutex::new(BTreeMap::new());

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
        let d_qif =
            make_detour(ntdll, b"NtQueryInformationFile\0", qif_hook as *const ())?;
        TRAMP_QIF = Some(core::mem::transmute::<*const (), NtQueryInformationFileFn>(
            d_qif.trampoline() as *const (),
        ));
        let d_setinfo =
            make_detour(ntdll, b"NtSetInformationFile\0", setinfo_hook as *const ())?;
        TRAMP_SETINFO = Some(core::mem::transmute::<*const (), NtSetInformationFileFn>(
            d_setinfo.trampoline() as *const (),
        ));

        d_create.enable().map_err(|_| InstallError::Detour)?;
        d_qattr.enable().map_err(|_| InstallError::Detour)?;
        d_qfull.enable().map_err(|_| InstallError::Detour)?;
        d_open.enable().map_err(|_| InstallError::Detour)?;
        d_qdirex.enable().map_err(|_| InstallError::Detour)?;
        d_close.enable().map_err(|_| InstallError::Detour)?;
        d_qif.enable().map_err(|_| InstallError::Detour)?;
        d_setinfo.enable().map_err(|_| InstallError::Detour)?;

        let mut detours =
            vec![d_create, d_qattr, d_qfull, d_open, d_qdirex, d_close, d_qif, d_setinfo];

        // Best-effort child-process propagation: hook CreateProcessInternalW so
        // spawned children get the shim too. Never fails install — if our own
        // DLL path or the kernelbase export can't be resolved (e.g. in-process
        // tests where "self" is the test exe, not a DLL), skip it silently.
        if let Some(dll) = self_dll_path() {
            let _ = SELF_DLL.set(dll);
            let mut kb = GetModuleHandleA(b"kernelbase.dll\0".as_ptr());
            if kb.is_null() {
                kb = GetModuleHandleA(b"kernel32.dll\0".as_ptr());
            }
            if !kb.is_null() {
                if let Ok(d_cpiw) =
                    make_detour(kb, b"CreateProcessInternalW\0", cpiw_hook as *const ())
                {
                    TRAMP_CPIW = Some(core::mem::transmute::<*const (), CreateProcessInternalWFn>(
                        d_cpiw.trampoline() as *const (),
                    ));
                    if d_cpiw.enable().is_ok() {
                        detours.push(d_cpiw);
                    }
                }
            }
        }

        Ok(HookGuard { _detours: detours })
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

/// Decode + ask the engine what to do with an open, given its access mask and
/// create disposition (write-path aware).
unsafe fn decision_for(oa: *const ObjectAttributes, access: u32, disposition: u32) -> Option<Decision> {
    let engine = ENGINE.get()?;
    let path = path_of(oa)?;
    Some(engine.decide_open(&path, access, disposition))
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

/// Record a redirected handle's virtual identity: after a successful redirected
/// open, map the handle to the volume-relative form of the ORIGINAL virtual path
/// (`oa` still holds it — only a local `new_oa` was rewritten). Reclaimed by
/// `NtClose`. Enables the `NtQueryInformationFile` class-48 spoof.
unsafe fn record_identity(
    file_handle: *mut HANDLE,
    oa: *const ObjectAttributes,
    status: NTSTATUS,
) {
    if status < 0 || file_handle.is_null() {
        return;
    }
    if let Some(path) = path_of(oa) {
        if let Ok(mut t) = IDENTITY_TABLE.lock() {
            t.insert(*file_handle as isize, nt_to_volume_relative(&path));
        }
    }
}

/// Record a successful under-root open's handle -> folded vpath components, so
/// a later handle-based delete/rename can act by vpath. Shared by both open
/// hooks across all decision branches.
unsafe fn record_path(file_handle: *mut HANDLE, oa: *const ObjectAttributes, status: NTSTATUS) {
    if status < 0 || file_handle.is_null() {
        return;
    }
    if let (Some(engine), Some(path)) = (ENGINE.get(), path_of(oa)) {
        if let Some(comps) = engine.remainder(&path) {
            if let Ok(mut t) = PATH_TABLE.lock() {
                t.insert(*file_handle as isize, comps);
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
    match decision_for(oa, access, disp) {
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
            record_identity(file_handle, oa, status);
            record_path(file_handle, oa, status);
            status
        }
        Some(Decision::Deny) => STATUS_OBJECT_NAME_NOT_FOUND,
        Some(Decision::PassThrough) | None => {
            let status =
                tramp(file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen);
            tag_under_root(file_handle, oa, status);
            record_path(file_handle, oa, status);
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
    // NtOpenFile has no disposition; it always opens existing (FILE_OPEN).
    match decision_for(oa, access, vfs_redirect::FILE_OPEN) {
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
            record_identity(file_handle, oa, status);
            record_path(file_handle, oa, status);
            status
        }
        Some(Decision::Deny) => STATUS_OBJECT_NAME_NOT_FOUND,
        Some(Decision::PassThrough) | None => {
            let status = tramp(file_handle, access, oa, iosb, share, opts);
            tag_under_root(file_handle, oa, status);
            record_path(file_handle, oa, status);
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
    if let Ok(mut t) = IDENTITY_TABLE.lock() {
        t.remove(&(handle as isize));
    }
    if let Ok(mut t) = PATH_TABLE.lock() {
        t.remove(&(handle as isize));
    }
    tramp(handle)
}

/// `NtSetInformationFile` hook. Converts a delete (FileDispositionInformation /
/// ...Ex with the DELETE flag) of a tracked under-root handle into an overlay
/// whiteout and suppresses the real delete, so the mod backing / real file is
/// preserved but the path reads as gone. Everything else passes through
/// (rename handling is a later phase).
unsafe extern "system" fn setinfo_hook(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let tramp = match TRAMP_SETINFO {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    let is_delete = !info.is_null()
        && match class {
            FILE_DISPOSITION_INFORMATION => length >= 1 && *(info as *const u8) != 0,
            FILE_DISPOSITION_INFORMATION_EX => {
                length >= 4
                    && core::ptr::read_unaligned(info as *const u32) & FILE_DISPOSITION_DELETE != 0
            }
            _ => false,
        };
    if is_delete {
        let comps = match PATH_TABLE.lock() {
            Ok(t) => t.get(&(handle as isize)).cloned(),
            Err(_) => None,
        };
        if let (Some(comps), Some(engine)) = (comps, ENGINE.get()) {
            if engine.whiteout(&comps) {
                // Suppress the real delete; report success to the caller.
                if !iosb.is_null() {
                    let p = iosb as *mut u8;
                    core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
                    core::ptr::write_unaligned(p.add(8) as *mut usize, 0);
                }
                return STATUS_SUCCESS;
            }
        }
    }
    tramp(handle, iosb, info, length, class)
}

/// `NtQueryInformationFile` hook. Spoofs only `FileNormalizedNameInformation`
/// (class 48) on a redirected handle -> the virtual path, so
/// `GetFinalPathNameByHandleW` reports where the mod file appears to live.
/// Class 9 and everything else pass through (spoofing class 9 breaks
/// `GetFinalPathNameByHandleW`).
unsafe extern "system" fn qif_hook(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let tramp = match TRAMP_QIF {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if class == FILE_NORMALIZED_NAME_INFORMATION && !info.is_null() {
        let vpath = match IDENTITY_TABLE.lock() {
            Ok(t) => t.get(&(handle as isize)).cloned(),
            Err(_) => None,
        };
        if let Some(vpath) = vpath {
            let buf = core::slice::from_raw_parts_mut(info as *mut u8, length as usize);
            let r = write_file_name_info(&vpath, buf);
            let status = match r.status {
                DirStatus::Success => STATUS_SUCCESS,
                _ => STATUS_BUFFER_OVERFLOW,
            };
            if !iosb.is_null() {
                let p = iosb as *mut u8;
                core::ptr::write_unaligned(p as *mut u32, status as u32);
                core::ptr::write_unaligned(p.add(8) as *mut usize, r.bytes);
            }
            return status;
        }
    }
    tramp(handle, iosb, info, length, class)
}

/// `CreateProcessInternalW` hook: force the child to start suspended, inject the
/// shim, wait for its hooks to come up, then resume (unless the caller asked for
/// a suspended child). Best-effort — a failed inject or timeout still resumes
/// the child (unvirtualized rather than hung).
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn cpiw_hook(
    token: HANDLE,
    app: *const u16,
    cmd: *mut u16,
    proc_attr: *const c_void,
    thread_attr: *const c_void,
    inherit: i32,
    flags: u32,
    env: *const c_void,
    cur_dir: *const u16,
    si: *const STARTUPINFOW,
    pi: *mut PROCESS_INFORMATION,
    ptok: *mut HANDLE,
) -> i32 {
    let tramp = match TRAMP_CPIW {
        Some(t) => t,
        None => return 0, // STATUS/BOOL FALSE — invariant violation, should not occur
    };
    let caller_suspended = flags & CREATE_SUSPENDED != 0;
    let forced = flags | CREATE_SUSPENDED;
    let r = tramp(
        token, app, cmd, proc_attr, thread_attr, inherit, forced, env, cur_dir, si, pi, ptok,
    );
    if r != 0 && !pi.is_null() {
        let pid = (*pi).dwProcessId;
        let hprocess = (*pi).hProcess;
        let hthread = (*pi).hThread;
        if let Some(dll) = SELF_DLL.get() {
            if inject_dll(hprocess, dll) {
                // Give the child's shim a chance to install hooks before it runs.
                wait_ready(pid, CHILD_READY_TIMEOUT_MS);
            }
        }
        if !caller_suspended {
            ResumeThread(hthread);
        }
    }
    r
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
