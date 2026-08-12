//! The ntdll detours. ALL `unsafe` in the crate lives here.
#![allow(unsafe_code)]

use core::cell::Cell;
use core::ffi::c_void;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

/// Host-side metadata probes from inside hooks must not re-enter detours
/// (`Path::is_file` → NtCreateFile → try_fuse_create → … → stack overflow).
thread_local! {
    static HOOK_REENTER: Cell<u32> = const { Cell::new(0) };
}

fn hook_reenter_begin() -> bool {
    HOOK_REENTER.with(|c| {
        let d = c.get();
        if d > 0 {
            return false;
        }
        c.set(d + 1);
        true
    })
}

fn hook_reenter_end() {
    HOOK_REENTER.with(|c| {
        let d = c.get();
        c.set(d.saturating_sub(1));
    });
}

fn in_hook_reenter() -> bool {
    HOOK_REENTER.with(|c| c.get() > 0)
}

/// Opt-in only: when `VFS_ALLOW_DISK_FALLTHROUGH=1`, under-root FUSE NOT_FOUND
/// may open the host path (legacy / debug). Default **off** — game content must
/// come from the director (zip/overrides), never the Steam library tree.
fn allow_disk_fallthrough() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| match std::env::var("VFS_ALLOW_DISK_FALLTHROUGH") {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"),
        Err(_) => false,
    })
}

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
use crate::inject::{inject_child, re_suspend, self_dll_path};
use crate::ntdef::{
    FileAttributeTagInformation, FileBasicInformation, FileFsDeviceInformation,
    FileEndOfFileInformation, FileInternalInformation, FileNetworkOpenInformation,
    FilePositionInformation,
    FileStandardInformation, NtCloseFn, NtCreateFileFn, NtCreateSectionFn, NtMapViewOfSectionFn,
    NtOpenFileFn, NtQueryAttributesFileFn, NtQueryDirectoryFileExFn, NtQueryFullAttributesFileFn,
    NtQueryInformationFileFn, NtQueryVolumeInformationFileFn, NtReadFileFn, NtSetInformationFileFn,
    NtWriteFileFn, NtUnmapViewOfSectionFn, ObjectAttributes, UnicodeString, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_TAG_INFORMATION, FILE_ALL_INFORMATION,
    FILE_BASIC_INFORMATION, FILE_CREATED, FILE_DEVICE_DISK, FILE_DIRECTORY_FILE,
    FILE_DISPOSITION_DELETE, FILE_DISPOSITION_INFORMATION,
    FILE_DISPOSITION_INFORMATION_EX, FILE_END_OF_FILE_INFORMATION, FILE_FS_DEVICE_INFORMATION,
    FILE_INTERNAL_INFORMATION,
    FILE_NETWORK_OPEN_INFORMATION, FILE_NORMALIZED_NAME_INFORMATION, FILE_POSITION_INFORMATION,
    FILE_RENAME_INFORMATION, FILE_RENAME_INFORMATION_EX, FILE_STANDARD_INFORMATION, SEC_IMAGE,
    SL_RESTART_SCAN, SL_RETURN_SINGLE_ENTRY, STATUS_BUFFER_OVERFLOW, STATUS_END_OF_FILE,
    STATUS_INVALID_FILE_FOR_SECTION, STATUS_INVALID_HANDLE, STATUS_NO_MORE_FILES,
    STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND, STATUS_SECTION_TOO_BIG,
    STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
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
static mut TRAMP_READ: Option<NtReadFileFn> = None;
static mut TRAMP_WRITE: Option<NtWriteFileFn> = None;
static mut TRAMP_CREATE_SECTION: Option<NtCreateSectionFn> = None;
static mut TRAMP_MAP_VIEW: Option<NtMapViewOfSectionFn> = None;
static mut TRAMP_UNMAP_VIEW: Option<NtUnmapViewOfSectionFn> = None;
static mut TRAMP_QVOL: Option<NtQueryVolumeInformationFileFn> = None;
static mut TRAMP_CPIW: Option<CreateProcessInternalWFn> = None;
static mut TRAMP_GMFW: Option<GetModuleFileNameWFn> = None;
static mut TRAMP_GMFA: Option<GetModuleFileNameAFn> = None;
static mut TRAMP_QFPIW: Option<QueryFullProcessImageNameWFn> = None;

type GetModuleFileNameWFn = unsafe extern "system" fn(HMODULE, *mut u16, u32) -> u32;
type GetModuleFileNameAFn = unsafe extern "system" fn(HMODULE, *mut u8, u32) -> u32;
type QueryFullProcessImageNameWFn =
    unsafe extern "system" fn(HANDLE, u32, *mut u16, *mut u32) -> i32;

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

/// Any under-root open's handle -> the NT path it was opened as, so a later
/// handle-based delete/rename (NtSetInformationFile) can act by path.
static PATH_TABLE: Mutex<BTreeMap<isize, String>> = Mutex::new(BTreeMap::new());

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

/// Install all detours backed by `engine` (in-process / no early payload).
/// Idempotent-guarded. Patches the four path/attr stubs itself.
pub fn install(engine: Engine) -> Result<HookGuard, InstallError> {
    ENGINE.set(engine).map_err(|_| InstallError::AlreadyInstalled)?;
    // SAFETY: ntdll lookup + detour install; each hook matches its ABI.
    unsafe { install_all_detours(true) }
}

/// Dual-layer install: early payload already owns open/create/qattr/qfull.
/// Wire trampolines to the early Config's tramp buffers, publish secondary
/// dispatch pointers into that Config, and detour only the remaining stubs.
///
/// `payload_cfg` is the reflectively-mapped early Config in this process.
pub fn install_late(
    engine: Engine,
    payload_cfg: *mut crate::payload_abi::PayloadConfig,
) -> Result<HookGuard, InstallError> {
    if payload_cfg.is_null() {
        return Err(InstallError::Detour);
    }
    ENGINE.set(engine).map_err(|_| InstallError::AlreadyInstalled)?;

    // SAFETY: cfg is the live early Config in this process; tramp addresses
    // are RWX pages the injector allocated; secondary pointers are our hooks.
    unsafe {
        let cfg = &mut *payload_cfg;
        // Call originals via the early payload's trampolines (real ntdll tails).
        TRAMP_CREATE = Some(core::mem::transmute::<usize, NtCreateFileFn>(cfg.create_tramp));
        TRAMP_OPEN = Some(core::mem::transmute::<usize, NtOpenFileFn>(cfg.open_tramp));
        TRAMP_QATTR =
            Some(core::mem::transmute::<usize, NtQueryAttributesFileFn>(cfg.qattr_tramp));
        TRAMP_QFULL =
            Some(core::mem::transmute::<usize, NtQueryFullAttributesFileFn>(cfg.qfull_tramp));

        // Publish secondary last-ish: hooks become Engine-backed for non-table paths.
        core::ptr::write_volatile(
            &mut cfg.secondary_create,
            create_hook as *const () as usize,
        );
        core::ptr::write_volatile(&mut cfg.secondary_open, open_hook as *const () as usize);
        core::ptr::write_volatile(&mut cfg.secondary_qattr, qattr_hook as *const () as usize);
        core::ptr::write_volatile(&mut cfg.secondary_qfull, qfull_hook as *const () as usize);

        // Do NOT patch the four early-owned stubs.
        install_all_detours(false)
    }
}

/// `patch_early_owned`: when true, also detour the four path/attr stubs
/// (standalone install). When false, only remainder detours (dual-layer).
unsafe fn install_all_detours(patch_early_owned: bool) -> Result<HookGuard, InstallError> {
    let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
    if ntdll.is_null() {
        return Err(InstallError::NtdllMissing);
    }

    let mut detours: Vec<RawDetour> = Vec::new();

    if patch_early_owned {
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
        d_create.enable().map_err(|_| InstallError::Detour)?;
        d_qattr.enable().map_err(|_| InstallError::Detour)?;
        d_qfull.enable().map_err(|_| InstallError::Detour)?;
        d_open.enable().map_err(|_| InstallError::Detour)?;
        detours.extend([d_create, d_qattr, d_qfull, d_open]);
    }

    let d_qdirex = make_detour(ntdll, b"NtQueryDirectoryFileEx\0", qdirex_hook as *const ())?;
    TRAMP_QDIREX = Some(core::mem::transmute::<*const (), NtQueryDirectoryFileExFn>(
        d_qdirex.trampoline() as *const (),
    ));
    let d_close = make_detour(ntdll, b"NtClose\0", close_hook as *const ())?;
    TRAMP_CLOSE = Some(core::mem::transmute::<*const (), NtCloseFn>(
        d_close.trampoline() as *const (),
    ));
    let d_qif = make_detour(ntdll, b"NtQueryInformationFile\0", qif_hook as *const ())?;
    TRAMP_QIF = Some(core::mem::transmute::<*const (), NtQueryInformationFileFn>(
        d_qif.trampoline() as *const (),
    ));
    let d_setinfo = make_detour(ntdll, b"NtSetInformationFile\0", setinfo_hook as *const ())?;
    TRAMP_SETINFO = Some(core::mem::transmute::<*const (), NtSetInformationFileFn>(
        d_setinfo.trampoline() as *const (),
    ));
    let d_read = make_detour(ntdll, b"NtReadFile\0", read_hook as *const ())?;
    TRAMP_READ = Some(core::mem::transmute::<*const (), NtReadFileFn>(
        d_read.trampoline() as *const (),
    ));
    let d_write = make_detour(ntdll, b"NtWriteFile\0", write_hook as *const ())?;
    TRAMP_WRITE = Some(core::mem::transmute::<*const (), NtWriteFileFn>(
        d_write.trampoline() as *const (),
    ));
    let d_csec = make_detour(ntdll, b"NtCreateSection\0", create_section_hook as *const ())?;
    TRAMP_CREATE_SECTION = Some(core::mem::transmute::<*const (), NtCreateSectionFn>(
        d_csec.trampoline() as *const (),
    ));
    let d_map = make_detour(ntdll, b"NtMapViewOfSection\0", map_view_hook as *const ())?;
    TRAMP_MAP_VIEW = Some(core::mem::transmute::<*const (), NtMapViewOfSectionFn>(
        d_map.trampoline() as *const (),
    ));
    let d_unmap = make_detour(ntdll, b"NtUnmapViewOfSection\0", unmap_view_hook as *const ())?;
    TRAMP_UNMAP_VIEW = Some(core::mem::transmute::<*const (), NtUnmapViewOfSectionFn>(
        d_unmap.trampoline() as *const (),
    ));
    let d_qvol =
        make_detour(ntdll, b"NtQueryVolumeInformationFile\0", qvol_hook as *const ())?;
    TRAMP_QVOL = Some(core::mem::transmute::<*const (), NtQueryVolumeInformationFileFn>(
        d_qvol.trampoline() as *const (),
    ));

    d_qdirex.enable().map_err(|_| InstallError::Detour)?;
    d_close.enable().map_err(|_| InstallError::Detour)?;
    d_qif.enable().map_err(|_| InstallError::Detour)?;
    d_setinfo.enable().map_err(|_| InstallError::Detour)?;
    d_read.enable().map_err(|_| InstallError::Detour)?;
    d_write.enable().map_err(|_| InstallError::Detour)?;
    d_csec.enable().map_err(|_| InstallError::Detour)?;
    d_map.enable().map_err(|_| InstallError::Detour)?;
    d_unmap.enable().map_err(|_| InstallError::Detour)?;
    d_qvol.enable().map_err(|_| InstallError::Detour)?;
    detours.extend([
        d_qdirex, d_close, d_qif, d_setinfo, d_read, d_write, d_csec, d_map, d_unmap, d_qvol,
    ]);

    // Best-effort child-process propagation + virtual image path spoof.
    if let Some(dll) = self_dll_path() {
        let _ = SELF_DLL.set(dll);
        let mut kb = GetModuleHandleA(b"kernelbase.dll\0".as_ptr());
        if kb.is_null() {
            kb = GetModuleHandleA(b"kernel32.dll\0".as_ptr());
        }
        if !kb.is_null() {
            if let Ok(d_cpiw) = make_detour(kb, b"CreateProcessInternalW\0", cpiw_hook as *const ())
            {
                TRAMP_CPIW = Some(core::mem::transmute::<*const (), CreateProcessInternalWFn>(
                    d_cpiw.trampoline() as *const (),
                ));
                if d_cpiw.enable().is_ok() {
                    detours.push(d_cpiw);
                }
            }
            // Spoof GetModuleFileName / QueryFullProcessImageName when the
            // process was hollowed from a zip PE (VFS_VIRTUAL_IMAGE). Without
            // this, SKSE/game see cmd.exe and abort ("outside of loader control").
            for mod_name in [b"kernel32.dll\0".as_slice(), b"kernelbase.dll\0".as_slice()] {
                let hm = GetModuleHandleA(mod_name.as_ptr());
                if hm.is_null() {
                    continue;
                }
                if TRAMP_GMFW.is_none() {
                    if let Ok(d) =
                        make_detour(hm, b"GetModuleFileNameW\0", gmf_hook as *const ())
                    {
                        TRAMP_GMFW = Some(core::mem::transmute::<*const (), GetModuleFileNameWFn>(
                            d.trampoline() as *const (),
                        ));
                        if d.enable().is_ok() {
                            detours.push(d);
                        }
                    }
                }
                if TRAMP_GMFA.is_none() {
                    if let Ok(d) =
                        make_detour(hm, b"GetModuleFileNameA\0", gmfa_hook as *const ())
                    {
                        TRAMP_GMFA = Some(core::mem::transmute::<*const (), GetModuleFileNameAFn>(
                            d.trampoline() as *const (),
                        ));
                        if d.enable().is_ok() {
                            detours.push(d);
                        }
                    }
                }
                if TRAMP_QFPIW.is_none() {
                    if let Ok(d) = make_detour(
                        hm,
                        b"QueryFullProcessImageNameW\0",
                        qfpi_hook as *const (),
                    ) {
                        TRAMP_QFPIW =
                            Some(core::mem::transmute::<*const (), QueryFullProcessImageNameWFn>(
                                d.trampoline() as *const (),
                            ));
                        if d.enable().is_ok() {
                            detours.push(d);
                        }
                    }
                }
            }
            let _ = kb;
        }
    }

    Ok(HookGuard { _detours: detours })
}

/// Spoof main-module path when hollowed from a zip PE (`VFS_VIRTUAL_IMAGE`) or
/// when the real path is an ephemeral `vfs-run-*` staging file.
fn should_spoof_image_path(real: &str) -> bool {
    if std::env::var_os("VFS_VIRTUAL_IMAGE").is_none() {
        return false;
    }
    let r = real.to_ascii_lowercase();
    // Always spoof sacrificial hosts. When host is Steam SkyrimSE, still spoof
    // to the VFS virtual path so Data/ resolves under GameLayers (zip serve),
    // while kernel ProcessImageFileName stays the Steam path for DRM.
    r.contains("vfs-run-")
        || r.contains("vfs-sse-")
        || r.contains("vfs-sec-")
        || r.ends_with("\\cmd.exe")
        || r.ends_with("\\runtimebroker.exe")
        || r.ends_with("\\notepad.exe")
        || r.ends_with("\\conhost.exe")
        || r.contains("\\system32\\cmd.exe")
        || r.contains("skyrimse.exe")
        || r.contains("skyrim special edition")
}

unsafe extern "system" fn gmf_hook(module: HMODULE, buffer: *mut u16, size: u32) -> u32 {
    let tramp = match TRAMP_GMFW {
        Some(t) => t,
        None => return 0,
    };
    let main = GetModuleHandleA(core::ptr::null());
    let is_main = module.is_null() || (!main.is_null() && module == main);
    if is_main {
        let real_n = tramp(module, buffer, size);
        if real_n > 0 && !buffer.is_null() {
            let real = String::from_utf16_lossy(core::slice::from_raw_parts(
                buffer,
                real_n as usize,
            ));
            if should_spoof_image_path(&real) {
                if let Ok(virt) = std::env::var("VFS_VIRTUAL_IMAGE") {
                    let units: Vec<u16> = virt.encode_utf16().collect();
                    if size as usize > units.len() {
                        core::ptr::copy_nonoverlapping(units.as_ptr(), buffer, units.len());
                        *buffer.add(units.len()) = 0;
                        return units.len() as u32;
                    }
                }
            }
            return real_n;
        }
    }
    tramp(module, buffer, size)
}

unsafe extern "system" fn gmfa_hook(module: HMODULE, buffer: *mut u8, size: u32) -> u32 {
    let tramp = match TRAMP_GMFA {
        Some(t) => t,
        None => return 0,
    };
    let main = GetModuleHandleA(core::ptr::null());
    let is_main = module.is_null() || (!main.is_null() && module == main);
    if is_main {
        let real_n = tramp(module, buffer, size);
        if real_n > 0 && !buffer.is_null() {
            let real = core::str::from_utf8(core::slice::from_raw_parts(buffer, real_n as usize))
                .unwrap_or("");
            if should_spoof_image_path(real) {
                if let Ok(virt) = std::env::var("VFS_VIRTUAL_IMAGE") {
                    let bytes = virt.as_bytes();
                    if size as usize > bytes.len() {
                        core::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
                        *buffer.add(bytes.len()) = 0;
                        return bytes.len() as u32;
                    }
                }
            }
            return real_n;
        }
    }
    tramp(module, buffer, size)
}

/// Spoof `QueryFullProcessImageNameW` for the current process.
unsafe extern "system" fn qfpi_hook(
    process: HANDLE,
    flags: u32,
    buffer: *mut u16,
    size: *mut u32,
) -> i32 {
    let tramp = match TRAMP_QFPIW {
        Some(t) => t,
        None => return 0,
    };
    // Current process: GetCurrentProcess() is (HANDLE)-1.
    let is_self = process == (-1isize as HANDLE) || process == (usize::MAX as HANDLE);
    if is_self {
        if let Ok(virt) = std::env::var("VFS_VIRTUAL_IMAGE") {
            if !buffer.is_null() && !size.is_null() {
                let units: Vec<u16> = virt.encode_utf16().collect();
                let need = units.len() as u32;
                if *size > need {
                    core::ptr::copy_nonoverlapping(units.as_ptr(), buffer, units.len());
                    *buffer.add(units.len()) = 0;
                    *size = need;
                    let _ = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(r"C:\GameLayers\vfs-state\gmf-spoof.log")
                        .and_then(|mut f| {
                            use std::io::Write;
                            writeln!(f, "qfpi spoof -> {virt}")
                        });
                    return 1; // TRUE
                }
            }
        }
    }
    tramp(process, flags, buffer, size)
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
        if engine.is_under_root(&path) {
            if let Ok(mut t) = PATH_TABLE.lock() {
                t.insert(*file_handle as isize, path);
            }
        }
    }
}

/// Parse the target path from a `FILE_RENAME_INFORMATION`(`_EX`) buffer. Only
/// absolute targets (RootDirectory == NULL) are handled; otherwise `None`.
unsafe fn parse_rename_target(info: *mut c_void, length: u32) -> Option<String> {
    let len = length as usize;
    if info.is_null() || len < 20 {
        return None;
    }
    let b = info as *const u8;
    let root_dir = core::ptr::read_unaligned(b.add(8) as *const usize);
    if root_dir != 0 {
        return None;
    }
    let namelen = core::ptr::read_unaligned(b.add(16) as *const u32) as usize;
    if 20 + namelen > len {
        return None;
    }
    let units = core::slice::from_raw_parts(b.add(20) as *const u16, namelen / 2);
    Some(String::from_utf16_lossy(units))
}

/// Try director FUSE OPEN for paths under the managed root. Returns Some(status)
/// when the fuse client handled the call (success or hard failure under root).
/// An under-root open needs the ring WRITE path when write access or a
/// create/overwrite disposition is present (SUPERSEDE/CREATE/OVERWRITE[_IF]).
fn is_write_open(access: u32, disposition: u32) -> bool {
    const WRITE_ACCESS: u32 = 0x4000_0000 | 0x0002 | 0x0004; // GENERIC_WRITE|FILE_WRITE_DATA|FILE_APPEND_DATA
    (access & WRITE_ACCESS) != 0 || matches!(disposition, 0 | 2 | 4 | 5)
}

unsafe fn try_fuse_create(
    file_handle: *mut HANDLE,
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    write: bool,
) -> Option<NTSTATUS> {
    let client = crate::fuse_client::global()?;
    let path = path_of(oa)?;
    let vpath = client.vpath_under_root(&path)?;
    // Directory open of root: empty vpath → "."
    let vp = if vpath.is_empty() { "." } else { vpath.as_str() };

    // All under-root *reads* go through the director (zip / composed sources).
    // Never open the Steam library tree for game content — that violates the
    // VFS goal. Writes may fall through for shim-local overlay redirect.
    let opened = if write { client.open_write(vp) } else { client.open(vp) };
    match opened {
        Ok(resp) => {
            let h = crate::fuse_synth::open_fuse(resp.fh, resp.size, resp.is_dir)?;
            if !file_handle.is_null() {
                *file_handle = h as HANDLE;
            }
            if !iosb.is_null() {
                let p = iosb as *mut u8;
                core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
                core::ptr::write_unaligned(p.add(8) as *mut usize, crate::ntdef::FILE_OPENED);
            }
            record_path(file_handle, oa, STATUS_SUCCESS);
            if resp.is_dir {
                tag_under_root(file_handle, oa, STATUS_SUCCESS);
            }
            director_open_trace(&path, resp.size);
            Some(STATUS_SUCCESS)
        }
        // Not in director: seal under-root *reads* (no Steam-disk fallthrough).
        // Writes fall through so overlay redirect / create still works.
        // steam_appid.txt must live in the overrides mount (skyrim-live writes it).
        Err(st) if st == vfs_protocol::ST_NOT_FOUND => {
            if write || allow_disk_fallthrough() {
                None
            } else {
                Some(STATUS_OBJECT_NAME_NOT_FOUND)
            }
        }
        // Director rejects OPEN_WRITE (overlay is shim-local). Fall through so
        // write/create under the root hits the overlay redirect path.
        Err(_) if write => None,
        Err(_) => {
            // Director down / I/O — do not fall through to the Steam tree.
            Some(STATUS_UNSUCCESSFUL)
        }
    }
}

/// Optional proof that opens went through the director (not host disk).
/// Set `VFS_DIRECTOR_OPEN_LOG` to a file path.
fn director_open_trace(nt_or_win_path: &str, size: u64) {
    let Some(path) = std::env::var_os("VFS_DIRECTOR_OPEN_LOG").map(std::path::PathBuf::from) else {
        return;
    };
    let p = crate::fuse_client::strip_nt_device(nt_or_win_path.trim()).replace('/', "\\");
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
    let line = format!(
        "{}\tdirector-open\tsize={}\t{}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        size,
        p
    );
    if !hook_reenter_begin() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
    hook_reenter_end();
}

/// Create a virtual directory under the managed root via the ring (`OP_MKDIR`),
/// then hand back a virtual directory handle. Only acts on directory opens
/// (`FILE_DIRECTORY_FILE`) with a creating disposition (CREATE / OPEN_IF /
/// OVERWRITE_IF); a plain FILE_OPEN of an existing dir is left to
/// `try_fuse_create`. Returns `None` when it doesn't apply (not under root, not
/// a dir create, or no FUSE client) so the caller falls through.
unsafe fn try_fuse_mkdir(
    file_handle: *mut HANDLE,
    oa: *const ObjectAttributes,
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
    let client = crate::fuse_client::global()?;
    let path = path_of(oa)?;
    let vpath = client.vpath_under_root(&path)?;
    let vp = if vpath.is_empty() { "." } else { vpath.as_str() };
    match client.mkdir(vp, 0o755) {
        Ok(()) => {
            // Synthesize a virtual directory handle directly — do NOT OP_OPEN the
            // new dir: the overlay opens paths as FileChannels, and a directory
            // open throws. fh=0 is never a real JVM handle, so the NtClose-time
            // close(0) is a harmless no-op. The caller (CreateDirectoryW) only
            // needs a handle to receive and immediately close; later metadata
            // reads are path-based (qattr/getattr), not through this handle.
            let h = crate::fuse_synth::open_fuse(0, 0, true)?;
            if !file_handle.is_null() {
                *file_handle = h as HANDLE;
            }
            if !iosb.is_null() {
                let p = iosb as *mut u8;
                core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
                core::ptr::write_unaligned(p.add(8) as *mut usize, FILE_CREATED);
            }
            record_path(file_handle, oa, STATUS_SUCCESS);
            tag_under_root(file_handle, oa, STATUS_SUCCESS);
            Some(STATUS_SUCCESS)
        }
        // Parent missing → name-not-found (do not fall through to a real on-disk
        // mkdir under the root).
        Err(st) if st == vfs_protocol::ST_NOT_FOUND => Some(STATUS_OBJECT_NAME_NOT_FOUND),
        // mkdir failed — most often the directory already exists (the overlay
        // raises :already-exists, which the JVM has no dedicated status for and
        // reports as a generic error). Probe: if a directory is really there,
        // honor the disposition — FILE_CREATE(2) must report a name collision
        // (ERROR_ALREADY_EXISTS, so the create-and-ignore idiom works), while
        // FILE_OPEN_IF(3)/FILE_OVERWRITE_IF(5) open the existing directory.
        Err(_) => match client.getattr(vp) {
            Ok(a) if a.found && a.is_dir => {
                if disp == 2 {
                    Some(STATUS_OBJECT_NAME_COLLISION)
                } else {
                    let h = crate::fuse_synth::open_fuse(0, 0, true)?;
                    if !file_handle.is_null() {
                        *file_handle = h as HANDLE;
                    }
                    if !iosb.is_null() {
                        let p = iosb as *mut u8;
                        core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
                        core::ptr::write_unaligned(p.add(8) as *mut usize, crate::ntdef::FILE_OPENED);
                    }
                    record_path(file_handle, oa, STATUS_SUCCESS);
                    tag_under_root(file_handle, oa, STATUS_SUCCESS);
                    Some(STATUS_SUCCESS)
                }
            }
            _ => Some(STATUS_UNSUCCESSFUL),
        },
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
    // Re-entrant host probes (is_file / log append) must hit the real ntdll.
    if in_hook_reenter() {
        return tramp(
            file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen,
        );
    }
    // Directory create under the managed root → ring OP_MKDIR (must precede the
    // generic file open below, which would otherwise create a FILE named as the
    // directory via the write-create path).
    if let Some(st) = try_fuse_mkdir(file_handle, oa, iosb, opts, disp) {
        return st;
    }
    // Prefer director FUSE for managed-root content (no in-shim zipserve).
    if let Some(st) = try_fuse_create(file_handle, oa, iosb, is_write_open(access, disp)) {
        return st;
    }
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
        Some(Decision::Serve { container_nt, offset, length }) => {
            // Legacy Serve path: only if FUSE client is not active.
            if crate::fuse_client::global().is_some() {
                return STATUS_OBJECT_NAME_NOT_FOUND;
            }
            match crate::zipserve::open_synth(&container_nt, offset, length) {
                Some(h) => {
                    if !file_handle.is_null() {
                        *file_handle = h as HANDLE;
                    }
                    if !iosb.is_null() {
                        let p = iosb as *mut u8;
                        core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
                        core::ptr::write_unaligned(
                            p.add(8) as *mut usize,
                            crate::ntdef::FILE_OPENED,
                        );
                    }
                    STATUS_SUCCESS
                }
                // Mapping failed: fall back to the real open (likely NOT_FOUND).
                None => tramp(
                    file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen,
                ),
            }
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
    if in_hook_reenter() {
        return tramp(file_handle, access, oa, iosb, share, opts);
    }
    // NtOpenFile has no disposition — it always opens existing (FILE_OPEN). Pass
    // FILE_OPEN (1), NOT 0: 0 is FILE_SUPERSEDE, which is in is_write_open's
    // create/overwrite set and would misclassify every open as a write.
    if let Some(st) = try_fuse_create(file_handle, oa, iosb, is_write_open(access, vfs_redirect::FILE_OPEN)) {
        return st;
    }
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
        Some(Decision::Serve { container_nt, offset, length }) => {
            if crate::fuse_client::global().is_some() {
                return STATUS_OBJECT_NAME_NOT_FOUND;
            }
            match crate::zipserve::open_synth(&container_nt, offset, length) {
                Some(h) => {
                    if !file_handle.is_null() {
                        *file_handle = h as HANDLE;
                    }
                    if !iosb.is_null() {
                        let p = iosb as *mut u8;
                        core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
                        core::ptr::write_unaligned(
                            p.add(8) as *mut usize,
                            crate::ntdef::FILE_OPENED,
                        );
                    }
                    STATUS_SUCCESS
                }
                None => tramp(file_handle, access, oa, iosb, share, opts),
            }
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

/// Path-based getattr via director OP_GETATTR when FUSE client is live.
/// `Some(...)` means the path is under the managed root — caller must not tramp
/// to the Steam tree on NOT_FOUND (seal under-root).
unsafe fn fuse_path_attr(path: &str) -> Option<Result<(bool, u64, i64), i32>> {
    let client = crate::fuse_client::global()?;
    let vpath = client.vpath_under_root(path)?;
    let vp = if vpath.is_empty() { "." } else { vpath.as_str() };
    Some(match client.getattr(vp) {
        Ok(a) if a.found => Ok((a.is_dir, a.size, a.mtime)),
        Ok(_) => Err(vfs_protocol::ST_NOT_FOUND),
        Err(st) => Err(st),
    })
}

unsafe extern "system" fn qattr_hook(
    oa: *const ObjectAttributes,
    info: *mut FileBasicInformation,
) -> NTSTATUS {
    let tramp = match TRAMP_QATTR {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if in_hook_reenter() {
        return tramp(oa, info);
    }
    if let Some(path) = path_of(oa) {
        // Under-root: director only (zip/overrides). Never host Steam metadata.
        if let Some(res) = fuse_path_attr(&path) {
            match res {
                Ok((is_dir, _size, _mtime)) => {
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
                Err(st) if st == vfs_protocol::ST_NOT_FOUND => {
                    if allow_disk_fallthrough() {
                        // fall through to engine / tramp
                    } else {
                        return STATUS_OBJECT_NAME_NOT_FOUND;
                    }
                }
                Err(_) => return STATUS_UNSUCCESSFUL,
            }
        }
        if let Some(engine) = ENGINE.get() {
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
    if in_hook_reenter() {
        return tramp(oa, info);
    }
    if let Some(path) = path_of(oa) {
        if let Some(res) = fuse_path_attr(&path) {
            match res {
                Ok((is_dir, size, _mtime)) => {
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
                Err(st) if st == vfs_protocol::ST_NOT_FOUND => {
                    if !allow_disk_fallthrough() {
                        return STATUS_OBJECT_NAME_NOT_FOUND;
                    }
                }
                Err(_) => return STATUS_UNSUCCESSFUL,
            }
        }
        if let Some(engine) = ENGINE.get() {
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
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        if let Some(fh) = crate::fuse_synth::close_fuse(handle as isize) {
            if let Some(c) = crate::fuse_client::global() {
                let _ = c.close(fh);
            }
        }
        return STATUS_SUCCESS;
    }
    if crate::zipserve::is_synth(handle as isize) {
        crate::zipserve::close(handle as isize);
        return STATUS_SUCCESS;
    }
    if crate::zipserve::is_synth_section(handle as isize) {
        crate::zipserve::close_section(handle as isize);
        return STATUS_SUCCESS;
    }
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

/// True when this `NtSetInformationFile` call requests a delete (either
/// disposition class with the delete flag/boolean set).
unsafe fn is_delete_request(info: *mut c_void, length: u32, class: u32) -> bool {
    !info.is_null()
        && match class {
            FILE_DISPOSITION_INFORMATION => length >= 1 && *(info as *const u8) != 0,
            FILE_DISPOSITION_INFORMATION_EX => {
                length >= 4
                    && core::ptr::read_unaligned(info as *const u32) & FILE_DISPOSITION_DELETE != 0
            }
            _ => false,
        }
}

/// Write a successful (Information = 0) IoStatusBlock for a set-info we handled
/// and suppressed from the real filesystem.
unsafe fn setinfo_ok_iosb(iosb: *mut c_void) {
    if !iosb.is_null() {
        let p = iosb as *mut u8;
        core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
        core::ptr::write_unaligned(p.add(8) as *mut usize, 0);
    }
}

/// `NtSetInformationFile` hook. For director FUSE (pure-ring) virtual handles it
/// routes truncate (`FileEndOfFileInformation`), delete, and rename to the JVM
/// overlay over the ring. For legacy local-overlay handles it converts a delete
/// or rename of a tracked under-root handle into an overlay whiteout/rename and
/// suppresses the real operation, so the mod backing / real file is preserved
/// but the path reads as gone/moved. Everything else passes through.
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
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        if class == FILE_POSITION_INFORMATION
            && !info.is_null()
            && length as usize >= core::mem::size_of::<FilePositionInformation>()
        {
            let pos = (*(info as *const FilePositionInformation)).current_byte_offset;
            if pos >= 0 {
                crate::fuse_synth::set_position(handle as isize, pos as u64);
            }
            return STATUS_SUCCESS;
        }
        // Truncate (`File::set_len`) → ring OP_SETATTR on the virtual write handle.
        if class == FILE_END_OF_FILE_INFORMATION
            && !info.is_null()
            && length as usize >= core::mem::size_of::<FileEndOfFileInformation>()
        {
            let eof = (*(info as *const FileEndOfFileInformation)).end_of_file;
            if let (Some((fh, _, _, _)), Some(c)) = (
                crate::fuse_synth::lookup(handle as isize),
                crate::fuse_client::global(),
            ) {
                if eof >= 0 && c.truncate(fh, eof as u64).is_ok() {
                    crate::fuse_synth::set_size(handle as isize, eof as u64);
                    setinfo_ok_iosb(iosb);
                    return STATUS_SUCCESS;
                }
            }
            return STATUS_UNSUCCESSFUL;
        }
        // Delete / rename of a virtual handle → ring OP_DELETE / OP_RENAME, keyed
        // by the NT path recorded (record_path) when the handle was opened.
        let is_delete = is_delete_request(info, length, class);
        let is_rename = matches!(class, FILE_RENAME_INFORMATION | FILE_RENAME_INFORMATION_EX);
        if is_delete || is_rename {
            let nt = match PATH_TABLE.lock() {
                Ok(t) => t.get(&(handle as isize)).cloned(),
                Err(_) => None,
            };
            if let (Some(nt), Some(c)) = (nt, crate::fuse_client::global()) {
                if let Some(vpath) = c.vpath_under_root(&nt) {
                    let src = if vpath.is_empty() { ".".to_string() } else { vpath };
                    let ok = if is_delete {
                        c.delete(&src).is_ok()
                    } else {
                        match parse_rename_target(info, length)
                            .and_then(|t| c.vpath_under_root(&t))
                        {
                            Some(dstv) => {
                                let dst = if dstv.is_empty() { ".".to_string() } else { dstv };
                                c.rename(&src, &dst).is_ok()
                            }
                            None => false,
                        }
                    };
                    if ok {
                        setinfo_ok_iosb(iosb);
                        return STATUS_SUCCESS;
                    }
                    return STATUS_UNSUCCESSFUL;
                }
            }
        }
        return STATUS_SUCCESS;
    }
    if crate::zipserve::is_synth(handle as isize) {
        if class == FILE_POSITION_INFORMATION
            && !info.is_null()
            && length as usize >= core::mem::size_of::<FilePositionInformation>()
        {
            let pos = (*(info as *const FilePositionInformation)).current_byte_offset;
            if pos >= 0 {
                crate::zipserve::set_position(handle as isize, pos as u64);
            }
            return STATUS_SUCCESS;
        }
        return STATUS_SUCCESS; // ignore other classes on synthetic handles
    }
    let is_delete = is_delete_request(info, length, class);
    let is_rename = matches!(class, FILE_RENAME_INFORMATION | FILE_RENAME_INFORMATION_EX);

    if is_delete || is_rename {
        let nt = match PATH_TABLE.lock() {
            Ok(t) => t.get(&(handle as isize)).cloned(),
            Err(_) => None,
        };
        if let (Some(nt), Some(engine)) = (nt, ENGINE.get()) {
            let handled = if is_delete {
                engine.whiteout(&nt)
            } else {
                match parse_rename_target(info, length) {
                    Some(target) => engine.rename(&nt, &target),
                    None => false,
                }
            };
            if handled {
                // Suppress the real delete/rename; report success to the caller.
                setinfo_ok_iosb(iosb);
                return STATUS_SUCCESS;
            }
        }
    }
    tramp(handle, iosb, info, length, class)
}

/// Fill IoStatusBlock for a successful synth query of `bytes` information.
unsafe fn synth_iosb_ok(iosb: *mut c_void, bytes: usize) {
    if !iosb.is_null() {
        let p = iosb as *mut u8;
        core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
        core::ptr::write_unaligned(p.add(8) as *mut usize, bytes);
    }
}

/// Answer handle-based information queries for director FUSE synth handles.
unsafe fn fuse_query_information(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let Some((fh, size, is_dir, pos)) = crate::fuse_synth::lookup(handle as isize) else {
        return STATUS_INVALID_HANDLE;
    };
    let _ = fh;
    if info.is_null() {
        return STATUS_UNSUCCESSFUL;
    }
    match class {
        FILE_BASIC_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileBasicInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            let bi = info as *mut FileBasicInformation;
            (*bi).creation_time = 0;
            (*bi).last_access_time = 0;
            (*bi).last_write_time = 0;
            (*bi).change_time = 0;
            (*bi).file_attributes =
                if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
            (*bi)._reserved = 0;
            synth_iosb_ok(iosb, core::mem::size_of::<FileBasicInformation>());
            STATUS_SUCCESS
        }
        FILE_STANDARD_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileStandardInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            let si = info as *mut FileStandardInformation;
            (*si).allocation_size = size as i64;
            (*si).end_of_file = size as i64;
            (*si).number_of_links = 1;
            (*si).delete_pending = 0;
            (*si).directory = if is_dir { 1 } else { 0 };
            (*si)._pad = 0;
            synth_iosb_ok(iosb, core::mem::size_of::<FileStandardInformation>());
            STATUS_SUCCESS
        }
        FILE_INTERNAL_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileInternalInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            (*(info as *mut FileInternalInformation)).index_number = handle as i64;
            synth_iosb_ok(iosb, core::mem::size_of::<FileInternalInformation>());
            STATUS_SUCCESS
        }
        FILE_POSITION_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FilePositionInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            (*(info as *mut FilePositionInformation)).current_byte_offset = pos as i64;
            synth_iosb_ok(iosb, core::mem::size_of::<FilePositionInformation>());
            STATUS_SUCCESS
        }
        FILE_NETWORK_OPEN_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileNetworkOpenInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            let ni = info as *mut FileNetworkOpenInformation;
            (*ni).creation_time = 0;
            (*ni).last_access_time = 0;
            (*ni).last_write_time = 0;
            (*ni).change_time = 0;
            (*ni).allocation_size = size as i64;
            (*ni).end_of_file = size as i64;
            (*ni).file_attributes =
                if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
            synth_iosb_ok(iosb, core::mem::size_of::<FileNetworkOpenInformation>());
            STATUS_SUCCESS
        }
        FILE_ALL_INFORMATION => {
            // GetFileInformationByHandle (Rust `metadata`) issues this. Fill the
            // fixed prefix callers read — attributes (incl. DIRECTORY), size, the
            // Standard.Directory flag — and leave the trailing name empty. Prefix
            // layout: Basic 40 | Standard 24 | Internal 8 | Ea 4 | Access 4 |
            // Position 8 | Mode 4 | Alignment 4 | Name 4 = 100.
            const PREFIX: usize = 100;
            if (length as usize) < PREFIX {
                return STATUS_BUFFER_OVERFLOW;
            }
            let p = info as *mut u8;
            core::ptr::write_bytes(p, 0, PREFIX);
            let attrs = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
            // Basic.FileAttributes @ 32
            core::ptr::write_unaligned(p.add(32) as *mut u32, attrs);
            // Standard.AllocationSize @ 40, EndOfFile @ 48, NumberOfLinks @ 56
            core::ptr::write_unaligned(p.add(40) as *mut i64, size as i64);
            core::ptr::write_unaligned(p.add(48) as *mut i64, size as i64);
            core::ptr::write_unaligned(p.add(56) as *mut u32, 1);
            // Standard.Directory (BOOLEAN) @ 61
            *p.add(61) = if is_dir { 1 } else { 0 };
            // Internal.IndexNumber @ 64
            core::ptr::write_unaligned(p.add(64) as *mut i64, handle as i64);
            // Position.CurrentByteOffset @ 80
            core::ptr::write_unaligned(p.add(80) as *mut i64, pos as i64);
            synth_iosb_ok(iosb, PREFIX);
            STATUS_SUCCESS
        }
        _ => {
            synth_iosb_ok(iosb, 0);
            STATUS_SUCCESS
        }
    }
}

/// Answer handle-based information queries for zip-window synthetic files.
/// Covers the classes `GetFileInformationByHandle` / Rust `metadata` use.
unsafe fn synth_query_information(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let Some(len) = crate::zipserve::size(handle as isize) else {
        return STATUS_INVALID_HANDLE;
    };
    let pos = crate::zipserve::position(handle as isize).unwrap_or(0);
    if info.is_null() {
        return STATUS_UNSUCCESSFUL;
    }

    match class {
        FILE_BASIC_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileBasicInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            let bi = info as *mut FileBasicInformation;
            (*bi).creation_time = 0;
            (*bi).last_access_time = 0;
            (*bi).last_write_time = 0;
            (*bi).change_time = 0;
            (*bi).file_attributes = FILE_ATTRIBUTE_NORMAL;
            (*bi)._reserved = 0;
            synth_iosb_ok(iosb, core::mem::size_of::<FileBasicInformation>());
            STATUS_SUCCESS
        }
        FILE_STANDARD_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileStandardInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            let si = info as *mut FileStandardInformation;
            (*si).allocation_size = len as i64;
            (*si).end_of_file = len as i64;
            (*si).number_of_links = 1;
            (*si).delete_pending = 0;
            (*si).directory = 0;
            (*si)._pad = 0;
            synth_iosb_ok(iosb, core::mem::size_of::<FileStandardInformation>());
            STATUS_SUCCESS
        }
        FILE_INTERNAL_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileInternalInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            // Stable fake index derived from the synthetic handle value.
            (*(info as *mut FileInternalInformation)).index_number = handle as i64;
            synth_iosb_ok(iosb, core::mem::size_of::<FileInternalInformation>());
            STATUS_SUCCESS
        }
        FILE_POSITION_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FilePositionInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            (*(info as *mut FilePositionInformation)).current_byte_offset = pos as i64;
            synth_iosb_ok(iosb, core::mem::size_of::<FilePositionInformation>());
            STATUS_SUCCESS
        }
        FILE_NETWORK_OPEN_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileNetworkOpenInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            let ni = info as *mut FileNetworkOpenInformation;
            (*ni).creation_time = 0;
            (*ni).last_access_time = 0;
            (*ni).last_write_time = 0;
            (*ni).change_time = 0;
            (*ni).allocation_size = len as i64;
            (*ni).end_of_file = len as i64;
            (*ni).file_attributes = FILE_ATTRIBUTE_NORMAL;
            (*ni)._reserved = 0;
            synth_iosb_ok(iosb, core::mem::size_of::<FileNetworkOpenInformation>());
            STATUS_SUCCESS
        }
        FILE_ATTRIBUTE_TAG_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileAttributeTagInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            let at = info as *mut FileAttributeTagInformation;
            (*at).file_attributes = FILE_ATTRIBUTE_NORMAL;
            (*at).reparse_tag = 0;
            synth_iosb_ok(iosb, core::mem::size_of::<FileAttributeTagInformation>());
            STATUS_SUCCESS
        }
        FILE_ALL_INFORMATION => {
            // FILE_ALL_INFORMATION starts with BASIC + STANDARD + INTERNAL + EA +
            // ACCESS + POSITION + MODE + ALIGNMENT + NAME. We fill the fixed
            // header fields callers typically read (size / attributes) and leave
            // the trailing name empty. Minimum size for the fixed prefix:
            // Basic(40)+Standard(24)+Internal(8)+Ea(4)+Access(4)+Position(8)+Mode(4)+Align(4)+Name(4) = 100.
            const PREFIX: usize = 100;
            if (length as usize) < PREFIX {
                return STATUS_BUFFER_OVERFLOW;
            }
            let p = info as *mut u8;
            core::ptr::write_bytes(p, 0, PREFIX);
            // Basic.FileAttributes @ offset 32
            core::ptr::write_unaligned(p.add(32) as *mut u32, FILE_ATTRIBUTE_NORMAL);
            // Standard.AllocationSize @ 40, EndOfFile @ 48
            core::ptr::write_unaligned(p.add(40) as *mut i64, len as i64);
            core::ptr::write_unaligned(p.add(48) as *mut i64, len as i64);
            // Standard.NumberOfLinks @ 56 = 1
            core::ptr::write_unaligned(p.add(56) as *mut u32, 1);
            // Internal.IndexNumber @ 64
            core::ptr::write_unaligned(p.add(64) as *mut i64, handle as i64);
            // Position.CurrentByteOffset @ 80 (after Ea 4 + Access 4 = 8 from 72)
            // Layout: Basic 40 | Std 24 | Int 8 | Ea 4 | Access 4 | Pos 8 | Mode 4 | Align 4 | Name…
            // Pos at 40+24+8+4+4 = 80
            core::ptr::write_unaligned(p.add(80) as *mut i64, pos as i64);
            synth_iosb_ok(iosb, PREFIX);
            STATUS_SUCCESS
        }
        // Unknown class on a synthetic handle: succeed as a soft no-op so
        // unhooked callers do not treat us as a hard failure when the class is
        // informational. Length-0 write keeps IoStatusBlock consistent.
        _ => {
            synth_iosb_ok(iosb, 0);
            STATUS_SUCCESS
        }
    }
}

/// `NtQueryVolumeInformationFile` hook — `GetFileType` needs
/// `FileFsDeviceInformation` on synthetic handles.
unsafe extern "system" fn qvol_hook(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let tramp = match TRAMP_QVOL {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize)
        || crate::zipserve::is_synth(handle as isize)
    {
        if class == FILE_FS_DEVICE_INFORMATION {
            if info.is_null() || (length as usize) < core::mem::size_of::<FileFsDeviceInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            let di = info as *mut FileFsDeviceInformation;
            (*di).device_type = FILE_DEVICE_DISK;
            (*di).characteristics = 0;
            synth_iosb_ok(iosb, core::mem::size_of::<FileFsDeviceInformation>());
            return STATUS_SUCCESS;
        }
        // Soft-success for other volume classes (size/attr) with zeros.
        if !info.is_null() && length > 0 {
            core::ptr::write_bytes(info as *mut u8, 0, length as usize);
        }
        synth_iosb_ok(iosb, length as usize);
        return STATUS_SUCCESS;
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
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        return fuse_query_information(handle, iosb, info, length, class);
    }
    if crate::zipserve::is_synth(handle as isize) {
        return synth_query_information(handle, iosb, info, length, class);
    }
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

/// `NtReadFile` hook. For synthetic (zip-window) handles, copy bytes from the
/// mapped window; real handles pass straight through. `ByteOffset` of NULL or
/// the "use current position" sentinel (-1/-2) means "current position".
/// `NtWriteFile` hook. For synthetic (fuse) write handles, forward the game's
/// buffer to the JVM overlay over the ring and complete the IRP; real handles
/// pass straight through. `ByteOffset` NULL / negative sentinel = current pos.
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn write_hook(
    handle: HANDLE,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    buffer: *mut c_void,
    length: u32,
    byte_offset: *const i64,
    key: *const u32,
) -> NTSTATUS {
    let tramp = match TRAMP_WRITE {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        let explicit = if byte_offset.is_null() {
            None
        } else {
            let v = core::ptr::read_unaligned(byte_offset);
            if v < 0 {
                None
            } else {
                Some(v as u64)
            }
        };
        if let Some((fh, _size, _is_dir, pos)) = crate::fuse_synth::lookup(handle as isize) {
            let off = explicit.unwrap_or(pos);
            let want = length as usize;
            let n = if want == 0 || buffer.is_null() {
                0usize
            } else {
                // SAFETY: NtWriteFile contract — buffer is readable for `length` bytes.
                let slice = unsafe { core::slice::from_raw_parts(buffer as *const u8, want) };
                match crate::fuse_client::global()
                    .ok_or(vfs_protocol::ST_IO_ERROR)
                    .and_then(|c| c.write(fh, off, slice))
                {
                    Ok(n) => n,
                    Err(_) => {
                        if !iosb.is_null() {
                            let p = iosb as *mut u8;
                            core::ptr::write_unaligned(p as *mut u32, STATUS_UNSUCCESSFUL as u32);
                            core::ptr::write_unaligned(p.add(8) as *mut usize, 0usize);
                        }
                        return STATUS_UNSUCCESSFUL;
                    }
                }
            };
            if explicit.is_none() {
                crate::fuse_synth::set_position(handle as isize, off + n as u64);
            }
            if !iosb.is_null() {
                let p = iosb as *mut u8;
                core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
                core::ptr::write_unaligned(p.add(8) as *mut usize, n);
            }
            if !event.is_null() {
                windows_sys::Win32::System::Threading::SetEvent(event);
            }
            let _ = (apc, apc_ctx, key);
            return STATUS_SUCCESS;
        }
        // Tagged synth handle with no table entry — never hand it to the real
        // NtWriteFile (mirrors read_hook).
        return STATUS_UNSUCCESSFUL;
    }
    tramp(
        handle, event, apc, apc_ctx, iosb, buffer, length, byte_offset, key,
    )
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn read_hook(
    handle: HANDLE,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    buffer: *mut c_void,
    length: u32,
    byte_offset: *const i64,
    key: *const u32,
) -> NTSTATUS {
    let tramp = match TRAMP_READ {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        let explicit = if byte_offset.is_null() {
            None
        } else {
            let v = core::ptr::read_unaligned(byte_offset);
            if v < 0 {
                None
            } else {
                Some(v as u64)
            }
        };
        if let Some((fh, size, _is_dir, pos)) = crate::fuse_synth::lookup(handle as isize) {
            let off = explicit.unwrap_or(pos);
            let want = length as usize;
            if off >= size {
                if !iosb.is_null() {
                    let p = iosb as *mut u8;
                    core::ptr::write_unaligned(p as *mut u32, STATUS_END_OF_FILE as u32);
                    core::ptr::write_unaligned(p.add(8) as *mut usize, 0usize);
                }
                return STATUS_END_OF_FILE;
            }
            // Phase 1: fill the game's NtReadFile buffer in place (no intermediate tmp).
            let max = want.min((size - off) as usize);
            let n = if max == 0 || buffer.is_null() {
                0usize
            } else {
                // SAFETY: NtReadFile contract — buffer is writable for `length` bytes.
                let slice =
                    unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, max) };
                match crate::fuse_client::global()
                    .ok_or(vfs_protocol::ST_IO_ERROR)
                    .and_then(|c| c.read_fragmented(fh, off, slice))
                {
                    Ok(n) => n,
                    Err(_) => {
                        if !iosb.is_null() {
                            let p = iosb as *mut u8;
                            core::ptr::write_unaligned(p as *mut u32, STATUS_UNSUCCESSFUL as u32);
                            core::ptr::write_unaligned(p.add(8) as *mut usize, 0usize);
                        }
                        return STATUS_UNSUCCESSFUL;
                    }
                }
            };
            {
                if explicit.is_none() {
                    crate::fuse_synth::set_position(handle as isize, off + n as u64);
                }
                let at_eof = off + n as u64 >= size;
                let status = if at_eof && n == 0 {
                    STATUS_END_OF_FILE
                } else {
                    STATUS_SUCCESS
                };
                if !iosb.is_null() {
                    let p = iosb as *mut u8;
                    core::ptr::write_unaligned(p as *mut u32, status as u32);
                    core::ptr::write_unaligned(p.add(8) as *mut usize, n);
                }
                if !event.is_null() {
                    windows_sys::Win32::System::Threading::SetEvent(event);
                }
                return status;
            }
        }
        return STATUS_UNSUCCESSFUL;
    }
    if crate::zipserve::is_synth(handle as isize) {
        // Resolve an explicit offset only if it is a real, non-sentinel value.
        let explicit = if byte_offset.is_null() {
            None
        } else {
            let v = core::ptr::read_unaligned(byte_offset);
            if v < 0 {
                None // FILE_USE_FILE_POINTER_POSITION and friends
            } else {
                Some(v as u64)
            }
        };
        match crate::zipserve::read(handle as isize, length as usize, explicit) {
            Some((bytes, _new_pos, at_eof)) => {
                if !buffer.is_null() && !bytes.is_empty() {
                    core::ptr::copy_nonoverlapping(
                        bytes.as_ptr(),
                        buffer as *mut u8,
                        bytes.len(),
                    );
                }
                let status = if at_eof { STATUS_END_OF_FILE } else { STATUS_SUCCESS };
                if !iosb.is_null() {
                    let p = iosb as *mut u8;
                    core::ptr::write_unaligned(p as *mut u32, status as u32);
                    core::ptr::write_unaligned(p.add(8) as *mut usize, bytes.len());
                }
                if !event.is_null() {
                    windows_sys::Win32::System::Threading::SetEvent(event);
                }
                return status;
            }
            None => {
                if !iosb.is_null() {
                    let p = iosb as *mut u8;
                    core::ptr::write_unaligned(p as *mut u32, STATUS_UNSUCCESSFUL as u32);
                    core::ptr::write_unaligned(p.add(8) as *mut usize, 0usize);
                }
                return STATUS_UNSUCCESSFUL;
            }
        }
    }
    tramp(handle, event, apc, apc_ctx, iosb, buffer, length, byte_offset, key)
}

/// Map a FUSE synthetic file into a synthetic section by streaming its contents
/// from the director (zip/composed sources) into a private mapping.
///
/// Skyrim BSAs exceed 1 GiB — we stream into `VirtualAlloc` (no intermediate
/// Vec) so CreateSection never opens the Steam library tree.
unsafe fn fuse_create_section(
    section_handle: *mut HANDLE,
    max_size: *mut i64,
    _page_prot: u32,
    alloc_attrs: u32,
    file_handle: HANDLE,
) -> NTSTATUS {
    // SEC_IMAGE not supported for FUSE data files (PEs use hollow/inject path).
    if alloc_attrs & SEC_IMAGE != 0 {
        return STATUS_INVALID_FILE_FOR_SECTION;
    }
    let Some((fh, size, is_dir, _)) = crate::fuse_synth::lookup(file_handle as isize) else {
        return STATUS_INVALID_HANDLE;
    };
    if is_dir || size == 0 {
        return STATUS_INVALID_FILE_FOR_SECTION;
    }
    // Cap: largest SE BSAs are ~1.7 GiB; leave headroom under 4 GiB usize maps.
    const MAX_MAP: u64 = 3 * 1024 * 1024 * 1024;
    if size > MAX_MAP {
        return STATUS_SECTION_TOO_BIG;
    }
    if !max_size.is_null() {
        let want = core::ptr::read_unaligned(max_size);
        if want > 0 && (want as u64) > size {
            return STATUS_SECTION_TOO_BIG;
        }
    }
    let Some(client) = crate::fuse_client::global() else {
        return STATUS_UNSUCCESSFUL;
    };
    use windows_sys::Win32::System::Memory::{
        VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
    };
    let map_len = size as usize;
    let base = VirtualAlloc(
        core::ptr::null(),
        map_len,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if base.is_null() {
        return STATUS_UNSUCCESSFUL;
    }
    let dest = core::slice::from_raw_parts_mut(base as *mut u8, map_len);
    match client.read_fragmented(fh, 0, dest) {
        Ok(n) if n == map_len => {}
        Ok(n) if n > 0 => {
            // Partial — still register mapped region of length n.
            let base2 = VirtualAlloc(
                core::ptr::null(),
                n,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            );
            if base2.is_null() {
                VirtualFree(base, 0, MEM_RELEASE);
                return STATUS_UNSUCCESSFUL;
            }
            core::ptr::copy_nonoverlapping(base as *const u8, base2 as *mut u8, n);
            VirtualFree(base, 0, MEM_RELEASE);
            return match crate::zipserve::register_mapped_image(base2 as usize, n as u64) {
                Some(h) => {
                    if !section_handle.is_null() {
                        *section_handle = h as HANDLE;
                    }
                    STATUS_SUCCESS
                }
                None => {
                    VirtualFree(base2, 0, MEM_RELEASE);
                    STATUS_INVALID_FILE_FOR_SECTION
                }
            };
        }
        _ => {
            VirtualFree(base, 0, MEM_RELEASE);
            return STATUS_UNSUCCESSFUL;
        }
    }
    match crate::zipserve::register_mapped_image(base as usize, size) {
        Some(h) => {
            if !section_handle.is_null() {
                *section_handle = h as HANDLE;
            }
            STATUS_SUCCESS
        }
        None => {
            VirtualFree(base, 0, MEM_RELEASE);
            STATUS_INVALID_FILE_FOR_SECTION
        }
    }
}

/// `NtCreateSection` hook: data sections over synthetic zip-window file handles
/// become synthetic section handles. `SEC_IMAGE` is rejected (PE images must be
/// real on-disk files). Real handles pass through.
unsafe extern "system" fn create_section_hook(
    section_handle: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    max_size: *mut i64,
    page_prot: u32,
    alloc_attrs: u32,
    file_handle: HANDLE,
) -> NTSTATUS {
    let tramp = match TRAMP_CREATE_SECTION {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // FUSE synthetic file handles: map by bulk-reading content (masters/BSAs).
    // Without this, NtCreateSection falls through to the kernel with a fake
    // handle and fails — Skyrim then never memory-maps ESMs.
    if crate::fuse_synth::is_fuse_synth(file_handle as isize) {
        return fuse_create_section(
            section_handle,
            max_size,
            page_prot,
            alloc_attrs,
            file_handle,
        );
    }
    if crate::zipserve::is_synth(file_handle as isize) {
        // SEC_IMAGE: map PE from zip window into this process (no disk staging).
        if alloc_attrs & SEC_IMAGE != 0 {
            let Some(len) = crate::zipserve::size(file_handle as isize) else {
                return STATUS_INVALID_HANDLE;
            };
            if len == 0 || len > 256 * 1024 * 1024 {
                return STATUS_INVALID_FILE_FOR_SECTION;
            }
            let Some((bytes, _, _)) =
                crate::zipserve::read(file_handle as isize, len as usize, Some(0))
            else {
                return STATUS_INVALID_HANDLE;
            };
            if !vfs_inject::pe_looks_like_image(&bytes) {
                return STATUS_INVALID_FILE_FOR_SECTION;
            }
            match vfs_inject::map_image_from_pe_bytes_local(&bytes) {
                Ok((base, size)) => {
                    // Register as a synthetic section whose MapView returns `base`.
                    match crate::zipserve::register_mapped_image(base as usize, size as u64) {
                        Some(h) => {
                            if !section_handle.is_null() {
                                *section_handle = h as HANDLE;
                            }
                            return STATUS_SUCCESS;
                        }
                        None => return STATUS_INVALID_FILE_FOR_SECTION,
                    }
                }
                Err(_) => return STATUS_INVALID_FILE_FOR_SECTION,
            }
        }
        // Optional MaximumSize must not exceed the window length.
        if let Some(len) = crate::zipserve::size(file_handle as isize) {
            if !max_size.is_null() {
                let want = core::ptr::read_unaligned(max_size);
                if want > 0 && (want as u64) > len {
                    return STATUS_SECTION_TOO_BIG;
                }
            }
        }
        match crate::zipserve::create_section(file_handle as isize) {
            Some(h) => {
                if !section_handle.is_null() {
                    *section_handle = h as HANDLE;
                }
                STATUS_SUCCESS
            }
            None => STATUS_INVALID_HANDLE,
        }
    } else {
        tramp(
            section_handle,
            access,
            oa,
            max_size,
            page_prot,
            alloc_attrs,
            file_handle,
        )
    }
}

/// `NtMapViewOfSection` hook: synthetic sections return a pointer into the
/// already-mapped zip window. Real sections pass through.
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn map_view_hook(
    section: HANDLE,
    process: HANDLE,
    base_address: *mut *mut c_void,
    zero_bits: usize,
    commit_size: usize,
    section_offset: *mut i64,
    view_size: *mut usize,
    inherit: u32,
    alloc_type: u32,
    protect: u32,
) -> NTSTATUS {
    let tramp = match TRAMP_MAP_VIEW {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::zipserve::is_synth_section(section as isize) {
        // Only the current process: cross-process map of our private VA is N/A.
        let off = if section_offset.is_null() {
            0u64
        } else {
            let v = core::ptr::read_unaligned(section_offset);
            if v < 0 {
                return STATUS_UNSUCCESSFUL;
            }
            v as u64
        };
        let want = if view_size.is_null() {
            0u64
        } else {
            *view_size as u64
        };
        match crate::zipserve::map_view(section as isize, off, want) {
            Some((base, size)) => {
                if !base_address.is_null() {
                    let preferred = *base_address;
                    if !preferred.is_null() && preferred as usize != base {
                        // Caller demanded a specific VA we cannot satisfy.
                        crate::zipserve::unmap_view(base);
                        return STATUS_UNSUCCESSFUL;
                    }
                    *base_address = base as *mut c_void;
                }
                if !view_size.is_null() {
                    *view_size = size as usize;
                }
                if !section_offset.is_null() {
                    core::ptr::write_unaligned(section_offset, off as i64);
                }
                let _ = (process, zero_bits, commit_size, inherit, alloc_type, protect);
                STATUS_SUCCESS
            }
            None => STATUS_UNSUCCESSFUL,
        }
    } else {
        tramp(
            section,
            process,
            base_address,
            zero_bits,
            commit_size,
            section_offset,
            view_size,
            inherit,
            alloc_type,
            protect,
        )
    }
}

/// `NtUnmapViewOfSection` hook: synthetic views are bookkeeping-only (the zip
/// map stays for the process lifetime).
unsafe extern "system" fn unmap_view_hook(process: HANDLE, base: *mut c_void) -> NTSTATUS {
    let tramp = match TRAMP_UNMAP_VIEW {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if !base.is_null() && crate::zipserve::is_synth_view(base as usize) {
        crate::zipserve::unmap_view(base as usize);
        let _ = process;
        return STATUS_SUCCESS;
    }
    tramp(process, base)
}

/// `CreateProcessInternalW` hook: force the child to start suspended, dual-layer
/// inject (early payload + full shim), wait for hooks, then resume (unless the
/// caller asked for a suspended child). Best-effort — a failed inject or timeout
/// still resumes the child (unvirtualized rather than hung).
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

    // CreateProcess maps the image in-kernel from a real path. Zip-window PEs
    // have no path — hollow from archive bytes (no filesystem write of PE).
    // Thread-local re-entry guard: create_process_from_pe_bytes itself calls
    // CreateProcessW(host) which must not re-enter hollow.
    thread_local! {
        static IN_HOLLOW_CREATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    let in_hollow_host = IN_HOLLOW_CREATE.with(|c| c.get());
    // Nested CreateProcess from create_process_from_pe_bytes (host EXE): pass
    // through with no inject/hollow — just create a clean suspended host.
    if in_hollow_host {
        return tramp(
            token, app, cmd, proc_attr, thread_attr, inherit, flags, env, cur_dir, si, pi, ptok,
        );
    }

    if let Some((virt_path, pe)) = resolve_virtual_pe(app, cmd) {
        let cwd = if cur_dir.is_null() {
            None
        } else {
            Some(utf16_z(cur_dir))
        };
        std::env::set_var("VFS_VIRTUAL_IMAGE", &virt_path);
        if let Some(parent) = std::path::Path::new(&virt_path).parent() {
            std::env::set_var("VFS_VIRTUAL_DIR", parent.to_string_lossy().as_ref());
        }
        // Inject shim *before* hollow so remote LoadLibrary of game-local
        // imports (steam_api64, bink, …) hits VFS Serve hooks.
        let dll = SELF_DLL.get().map(|s| s.as_str());
        IN_HOLLOW_CREATE.with(|c| c.set(true));
        let hollow_result = vfs_inject::create_process_from_pe_bytes_ex(
            &pe,
            &virt_path,
            &[],
            cwd.as_deref(),
            dll,
        );
        IN_HOLLOW_CREATE.with(|c| c.set(false));
        match hollow_result {
            Ok((hprocess, hthread, pid, tid)) => {
                eprintln!(
                    "vfs-shim: hollowed virtual PE {virt_path} -> pid={pid} ({} bytes)",
                    pe.len()
                );
                if !pi.is_null() {
                    (*pi).hProcess = hprocess;
                    (*pi).hThread = hthread;
                    (*pi).dwProcessId = pid;
                    (*pi).dwThreadId = tid;
                }
                // Do not LoadLibrary SKSE here — skse64_loader injects it into the
                // suspended child (CreateRemoteThread works while host stays mapped).
                // Shim already injected inside create_process_from_pe_bytes_ex.
                if caller_suspended {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                } else {
                    ResumeThread(hthread);
                }
                let _ = (token, proc_attr, thread_attr, inherit, env, si, ptok);
                return 1;
            }
            Err(e) => {
                eprintln!("vfs-shim: hollow CreateProcess failed: {e}");
                /* fall through */
            }
        }
    }

    let forced = flags | CREATE_SUSPENDED;
    let r = tramp(
        token, app, cmd, proc_attr, thread_attr, inherit, forced, env, cur_dir, si, pi, ptok,
    );
    if r != 0 && !pi.is_null() {
        let pid = (*pi).dwProcessId;
        let hprocess = (*pi).hProcess;
        let hthread = (*pi).hThread;
        if let Some(dll) = SELF_DLL.get() {
            let _ = inject_child(hprocess, hthread, pid, dll, CHILD_READY_TIMEOUT_MS);
            if caller_suspended {
                re_suspend(hthread);
            }
            if !caller_suspended {
                ResumeThread(hthread);
            }
        } else if !caller_suspended {
            ResumeThread(hthread);
        }
    }
    r
}

/// If CreateProcess targets a VFS zip-window PE, return (win32 path, pe bytes).
unsafe fn resolve_virtual_pe(app: *const u16, cmd: *mut u16) -> Option<(String, Vec<u8>)> {
    let engine = ENGINE.get()?;
    let mut path = if !app.is_null() {
        utf16_z(app)
    } else if !cmd.is_null() {
        // First token of the command line.
        let full = utf16_z(cmd);
        parse_first_arg(&full)?
    } else {
        return None;
    };
    // Normalize quotes.
    path = path.trim_matches('"').to_string();
    if path.is_empty() {
        return None;
    }
    let nt = if path.starts_with(r"\??\") || path.starts_with(r"\\?\") {
        path.clone()
    } else {
        format!(r"\??\{path}")
    };
    match engine.decide(&nt) {
        Decision::Serve {
            container_nt,
            offset,
            length,
        } => {
            if length == 0 || length > 256 * 1024 * 1024 {
                return None;
            }
            let pe = read_container_window(&container_nt, offset, length)?;
            if pe.len() < 2 || pe[0] != b'M' || pe[1] != b'Z' {
                return None;
            }
            Some((path, pe))
        }
        _ => None,
    }
}

fn parse_first_arg(cmd: &str) -> Option<String> {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return None;
    }
    if cmd.starts_with('"') {
        let end = cmd[1..].find('"')?;
        Some(cmd[1..1 + end].to_string())
    } else {
        Some(cmd.split_whitespace().next()?.to_string())
    }
}

unsafe fn utf16_z(p: *const u16) -> String {
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
        if len > 32_768 {
            break;
        }
    }
    String::from_utf16_lossy(core::slice::from_raw_parts(p, len))
}

fn read_container_window(container_nt: &str, offset: u64, length: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let path = container_nt
        .strip_prefix(r"\??\")
        .or_else(|| container_nt.strip_prefix(r"\\?\"))
        .unwrap_or(container_nt);
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = vec![0u8; length as usize];
    f.read_exact(&mut buf).ok()?;
    Some(buf)
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

    // Phase 2 (unlocked): build listing. Prefer director OP_READDIR when FUSE
    // is live (no in-shim snapshot merge). `drain_real` may call the syscall,
    // so the lock must NOT be held here (NtClose also takes it).
    let rebuilt = if need_build {
        let wildcard = wildcard_of(file_name);
        if let Some(client) = crate::fuse_client::global() {
            if let Some(vpath) = client.vpath_under_root(&dir_path) {
                let vp = if vpath.is_empty() { "." } else { vpath.as_str() };
                match client.readdir(vp) {
                    Ok(entries) => {
                        let mut items: Vec<DirItem> = entries
                            .into_iter()
                            .map(|e| DirItem {
                                name: e.name,
                                is_dir: e.is_dir,
                                size: e.size,
                                mtime: e.mtime,
                            })
                            .collect();
                        if let Some(ref w) = wildcard {
                            items.retain(|i| {
                                vfs_core::wildcard_match(w, &i.name)
                                    || i.name.eq_ignore_ascii_case(w)
                            });
                        }
                        Some(items)
                    }
                    Err(_) => Some(Vec::new()),
                }
            } else {
                let real = drain_real(handle, tramp);
                Some(match ENGINE.get() {
                    Some(engine) => engine.merge_directory(&dir_path, &real, wildcard.as_deref()),
                    None => real,
                })
            }
        } else {
            let real = drain_real(handle, tramp);
            Some(match ENGINE.get() {
                Some(engine) => engine.merge_directory(&dir_path, &real, wildcard.as_deref()),
                None => real,
            })
        }
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
