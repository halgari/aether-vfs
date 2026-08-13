//! The ntdll detours. ALL `unsafe` in the crate lives here.
#![allow(unsafe_code)]

use core::cell::Cell;
use core::ffi::c_void;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

// Host-side metadata probes from inside hooks must not re-enter detours
// (`Path::is_file` → NtCreateFile → try_fuse_create → … → stack overflow).
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
    *FLAG.get_or_init(|| vfs_env::opt_in(vfs_env::ALLOW_DISK_FALLTHROUGH))
}

/// Whether `steam_api*.dll` stays the host-install copy (excepted from the
/// director) rather than being served from the zip.
///
/// Must agree with `vfs_inject::keep_host_steam_api`: the two decide the same
/// question from opposite sides, and a disagreement leaves the module resolved
/// from one source and expected from the other.
///
/// Unset defaults to **true** here (unlike vfs-inject, which defaults false):
/// this exception used to be unconditional, so anything launching the shim
/// without setting the variable must keep seeing the host copy.
fn keep_host_steam_api() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| vfs_env::opt_out(vfs_env::KEEP_HOST_STEAM_API))
}

/// Whether a child process we inject starts with its working directory set to
/// the virtual root. Default **on**; `VFS_CHILD_CWD_ROOT=0` disables.
///
/// A launcher sets the child's cwd to its own directory — SKSE points it at the
/// staged launch dir. Two things then break: `SteamAPI_Init` reads
/// `steam_appid.txt` from the *cwd* and fails DRM with "Application load error
/// 3:0000065432" (a modal dialog, so the child hangs rather than exits), and the
/// game resolves `Data/` from there and finds no content. The virtual root is
/// where both actually live.
fn child_cwd_root() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| vfs_env::opt_out(vfs_env::CHILD_CWD_ROOT))
}

/// Experiment switch: when `VFS_FUSE_SKYRIM_EXE=1`, serve `SkyrimSE.exe`
/// through the director instead of excepting it to the host install.
///
/// **Measured 2026-08-12** (launch → main menu, `VFS_DRM_EXE_LOG` set): the
/// game process never opens `SkyrimSE.exe` through `try_fuse_create` at all —
/// the trace file was not created in either mode, and both runs reached the
/// menu with DRM satisfied and no "Steam Error". So this exception is inert on
/// the startup path and is *not* what fixes the historical symptom.
///
/// What actually needs the on-disk exe is outside this hook: `CreateProcess`
/// of the host image, and Steam's own path association (the client is a
/// separate, un-injected process, so our hooks cannot affect what it reads).
/// That association is what `ensure_canonical_skyrim_installdirs` addresses.
///
/// Kept default-**off** rather than deleted: the trace only covers startup, and
/// the recorded explanation for the original failure was wrong ("Steam hashes
/// the on-disk PE" — it does not; the whole loaded image was once rewritten in
/// memory and DRM still passed), so the real trigger may lie on a path not yet
/// exercised.
/// Only `SkyrimSE.exe` is affected; steam_api*, steam_appid.txt and the
/// launcher stay excepted.
fn fuse_skyrim_exe() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| vfs_env::opt_in(vfs_env::FUSE_SKYRIM_EXE))
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
    NtOpenFileFn, NtQueryAttributesFileFn, NtQueryDirectoryFileExFn, NtQueryDirectoryFileFn,
    NtQueryInformationByNameFn,
    NtQueryFullAttributesFileFn,
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
static mut TRAMP_QDIR: Option<NtQueryDirectoryFileFn> = None;
static mut TRAMP_QIBN: Option<NtQueryInformationByNameFn> = None;
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
    install_panic_hook();
    crate::hookstats::start_reporter();
    ENGINE.set(engine).map_err(|_| InstallError::AlreadyInstalled)?;
    // SAFETY: ntdll lookup + detour install; each hook matches its ABI.
    unsafe { install_all_detours(true) }
}

/// Record shim panics before they take the game down.
///
/// The workspace builds with `panic = "abort"`, and Rust's abort on MSVC is
/// `__fastfail(FAST_FAIL_FATAL_APP_EXIT)` — which surfaces as process exit code
/// **0xC0000409**. Without this hook every shim panic is an unattributable
/// `STATUS_STACK_BUFFER_OVERRUN`, indistinguishable from a genuine stack-cookie
/// or CFG failure in the game, and the only way to localise one is to bisect
/// (see the 0xC0000409 hunt behind commit 5f8f2eb).
///
/// `set_hook` still runs under `panic = "abort"`, so the message survives.
/// Writes to `VFS_SHIM_PANIC_LOG`, else `<state dir>/shim-panic.log`, else a
/// fixed fallback — a panic here must never be silent for want of a path.
fn install_panic_hook() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let default = vfs_env::text(vfs_env::STATE_DIR)
            .map(|d| format!("{d}\\shim-panic.log"));
        let path = vfs_env::text(vfs_env::SHIM_PANIC_LOG)
            .or(default)
            .unwrap_or_else(|| r"C:\tmp\skyrim-data\vfs-state\shim-panic.log".to_string());
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let loc = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_else(|| "<unknown location>".into());
            let msg = info.payload().downcast_ref::<&str>().map(|s| (*s).to_string())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string payload>".into());
            let line = format!(
                "pid={} tid={:?} at {loc}\n  {msg}\n",
                std::process::id(),
                std::thread::current().id()
            );
            // Guard the write: this file I/O re-enters our own NtCreateFile
            // hooks, and a panic raised *inside* a hook would otherwise recurse.
            if hook_reenter_begin() {
                if let Some(parent) = std::path::Path::new(&path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                use std::io::Write;
                if let Ok(mut f) =
                    std::fs::OpenOptions::new().create(true).append(true).open(&path)
                {
                    let _ = f.write_all(line.as_bytes());
                    let _ = f.flush();
                }
                hook_reenter_end();
            }
            // Best-effort stderr too; harmless when the child has no console.
            eprintln!("vfs-shim PANIC {line}");
            prev(info);
        }));
    });
}

/// Dual-layer install: early payload already owns open/create/qattr/qfull.
/// Wire trampolines to the early Config's tramp buffers, publish secondary
/// dispatch pointers into that Config, and detour only the remaining stubs.
///
/// `payload_cfg` is the reflectively-mapped early Config in this process.
///
/// # Safety
/// `payload_cfg` must point at a live [`PayloadConfig`](crate::payload_abi::PayloadConfig)
/// written by the injector into this process, and stay valid for the call.
pub unsafe fn install_late(
    engine: Engine,
    payload_cfg: *mut crate::payload_abi::PayloadConfig,
) -> Result<HookGuard, InstallError> {
    if payload_cfg.is_null() {
        return Err(InstallError::Detour);
    }
    install_panic_hook();
    crate::hookstats::start_reporter();
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
    // Both enumeration exports must be covered: whichever one the caller picks
    // decides whether it sees the composed tree or the real, near-empty folder
    // behind it, and a caller on the unhooked one leaves no trace anywhere.
    let d_qdir = make_detour(ntdll, b"NtQueryDirectoryFile\0", qdir_hook as *const ())?;
    TRAMP_QDIR = Some(core::mem::transmute::<*const (), NtQueryDirectoryFileFn>(
        d_qdir.trampoline() as *const (),
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

    // Present since Win10 1709. Optional so an older host still installs.
    if let Ok(d_qibn) = make_detour(ntdll, b"NtQueryInformationByName\0", qibn_hook as *const ()) {
        TRAMP_QIBN = Some(core::mem::transmute::<*const (), NtQueryInformationByNameFn>(
            d_qibn.trampoline() as *const (),
        ));
        if d_qibn.enable().is_ok() {
            detours.push(d_qibn);
        } else {
            TRAMP_QIBN = None;
        }
    }

    d_qdirex.enable().map_err(|_| InstallError::Detour)?;
    d_qdir.enable().map_err(|_| InstallError::Detour)?;
    d_close.enable().map_err(|_| InstallError::Detour)?;
    d_qif.enable().map_err(|_| InstallError::Detour)?;
    d_setinfo.enable().map_err(|_| InstallError::Detour)?;
    d_read.enable().map_err(|_| InstallError::Detour)?;
    d_write.enable().map_err(|_| InstallError::Detour)?;
    d_csec.enable().map_err(|_| InstallError::Detour)?;
    d_map.enable().map_err(|_| InstallError::Detour)?;
    d_unmap.enable().map_err(|_| InstallError::Detour)?;
    d_qvol.enable().map_err(|_| InstallError::Detour)?;
    // Every enabled detour must be kept alive here: dropping one silently
    // un-patches it, which reads exactly like "the process never calls this".
    detours.extend([
        d_qdirex, d_qdir, d_close, d_qif, d_setinfo, d_read, d_write, d_csec, d_map, d_unmap,
        d_qvol,
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
            let _ = kb;
        }
    }

    Ok(HookGuard { _detours: detours })
}

/// Decode ObjectName as UTF-16 (no root resolution).
unsafe fn object_name_str(oa: *const ObjectAttributes) -> Option<String> {
    if oa.is_null() {
        return None;
    }
    let oa_ref = &*oa;
    if oa_ref.object_name.is_null() {
        return None;
    }
    let us = &*oa_ref.object_name;
    if us.buffer.is_null() {
        return None;
    }
    let units = core::slice::from_raw_parts(us.buffer, us.length as usize / 2);
    Some(String::from_utf16_lossy(units))
}

/// Fully-qualified NT/Win32 path for an open.
///
/// Absolute names work as before. **Relative** opens (`RootDirectory` set) only
/// resolve when the root is a FUSE synthetic directory handle whose absolute
/// path was recorded in `PATH_TABLE`. Real kernel roots return `None` so the
/// caller can tramp. Without this, steam_api / CRT opens like
/// `RootDirectory=<game dir FUSE handle>, Name=steam_appid.txt` hit the kernel
/// with a fake handle → fail → **Steam Error**.
/// The `ObjectAttributes` name field alone, ignoring `RootDirectory`. Used only
/// to describe an open we could not resolve to a full path.
/// The process's current-directory handle and its DOS path, read from the PEB.
///
/// `RTL_USER_PROCESS_PARAMETERS.CurrentDirectory` is the only place the handle
/// is published; there is no API that hands it back.
unsafe fn cwd_from_peb() -> Option<(isize, String)> {
    // x64: TEB.ProcessEnvironmentBlock @ 0x60, PEB.ProcessParameters @ 0x20,
    // params.CurrentDirectory @ 0x38 = { UNICODE_STRING DosPath; HANDLE Handle }.
    let teb: usize;
    core::arch::asm!("mov {}, gs:[0x30]", out(reg) teb, options(nostack, preserves_flags));
    if teb == 0 {
        return None;
    }
    let peb = *((teb + 0x60) as *const usize);
    if peb == 0 {
        return None;
    }
    let params = *((peb + 0x20) as *const usize);
    if params == 0 {
        return None;
    }
    let units = *((params + 0x38) as *const u16) as usize / 2;
    let buf = *((params + 0x40) as *const *const u16);
    let handle = *((params + 0x48) as *const isize);
    if buf.is_null() || units == 0 || handle == 0 {
        return None;
    }
    Some((handle, String::from_utf16_lossy(core::slice::from_raw_parts(buf, units))))
}

/// The directory that a relative name is expressed against.
///
/// Three kinds of parent reach us, and missing any one makes the child
/// undecodable — which is silent rather than an error: the call simply bypasses
/// every decision we would have made and lands on whatever is really on disk.
/// Shared by every hook that has to decode a name, so they cannot drift apart.
unsafe fn parent_dir_of_handle(root_handle: HANDLE) -> Option<String> {
    let root = root_handle as isize;
    // 1. Our own synthetic directory handles.
    if crate::fuse_synth::is_fuse_synth(root) {
        // Prefer PATH_TABLE (recorded on open); fall back to fuse_synth abs_path.
        return PATH_TABLE
            .lock()
            .ok()
            .and_then(|t| t.get(&root).cloned())
            .or_else(|| crate::fuse_synth::abs_path(root));
    }
    // 2. A real directory the process opened; we remember every one.
    if let Some(p) = path_of_handle(root_handle) {
        return Some(p);
    }
    // 3. The current-directory handle. The OS creates it, so it is in no table
    //    of ours, yet it is the parent for every relative open a CRT makes:
    //    `CreateFileW("Data\X")` becomes (CWD handle + "Data\X").
    let (cwd_handle, dos) = cwd_from_peb()?;
    if cwd_handle != root {
        return None;
    }
    Some(format!(r"\??\{}", dos.trim_end_matches(['\\', '/'])))
}

unsafe fn oa_name_only(oa: *const ObjectAttributes) -> Option<String> {
    if oa.is_null() {
        return None;
    }
    object_name_str(oa)
}

unsafe fn path_of(oa: *const ObjectAttributes) -> Option<String> {
    if oa.is_null() {
        return None;
    }
    let oa_ref = &*oa;
    let name = object_name_str(oa)?;
    if oa_ref.root_directory.is_null() {
        return if name.is_empty() { None } else { Some(name) };
    }
    let parent = parent_dir_of_handle(oa_ref.root_directory)?;
    let parent = parent.trim_end_matches(['\\', '/']);
    let rel = name.trim_start_matches(['\\', '/']);
    if rel.is_empty() {
        Some(parent.to_string())
    } else {
        Some(format!("{parent}\\{rel}"))
    }
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
    let Some(path) = path_of(oa) else { return };
    let key = *file_handle as isize;
    // Remember every handle's path, not just the ones under the root. NT lets a
    // caller open a file as (directory handle + leaf name), and without the
    // parent's path such an open cannot be decoded at all -- it is invisible to
    // every decision we make and reaches the real directory behind the mount.
    // The parent is often outside the root while the child is under it.
    if let Ok(mut t) = HANDLE_PATHS.lock() {
        if t.len() < HANDLE_PATHS_MAX {
            t.insert(key, path.clone());
        }
    }
    if path_is_ours(&path) {
        if let Ok(mut table) = DIR_TABLE.lock() {
            table.insert(key, DirTracked { dir_nt_path: path, state: None });
        }
    }
}

/// Handle -> the NT path it was opened as, for *every* successful open.
///
/// Reclaimed by `NtClose`; bounded so a handle leak cannot grow it without end.
static HANDLE_PATHS: Mutex<BTreeMap<isize, String>> = Mutex::new(BTreeMap::new());
const HANDLE_PATHS_MAX: usize = 65_536;

fn path_of_handle(handle: HANDLE) -> Option<String> {
    HANDLE_PATHS.lock().ok()?.get(&(handle as isize)).cloned()
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
    if let Some(path) = path_of(oa) {
        if path_is_ours(&path) {
            if let Ok(mut t) = PATH_TABLE.lock() {
                t.insert(*file_handle as isize, path);
            }
        }
    }
}

/// Is this path one we are responsible for?
///
/// There are two notions of "ours" and they are not the same. The engine knows
/// the managed root; the client *also* serves the staging directory as an alias
/// for it — and a staged game's working directory is that staging directory, so
/// it reaches our content by that name. Anything that asks the narrower question
/// silently disowns the aliased half.
///
/// Every caller must ask through here. When `tag_under_root` asked the narrow
/// question, the enumeration of `<stage>\Data` went untracked and fell through
/// to the real staging folder, which returned nothing — and an empty `Data`
/// listing is an empty load order. The alias itself was unit-tested and correct;
/// what drifted was which callers consulted it.
fn path_is_ours(path: &str) -> bool {
    if ENGINE.get().is_some_and(|e| e.is_under_root(path)) {
        return true;
    }
    crate::fuse_client::global().is_some_and(|c| c.vpath_under_root(path).is_some())
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
    let namelen = core::ptr::read_unaligned(b.add(16) as *const u32) as usize;
    if 20 + namelen > len {
        return None;
    }
    let units = core::slice::from_raw_parts(b.add(20) as *const u16, namelen / 2);
    let name = String::from_utf16_lossy(units);
    if root_dir == 0 {
        return Some(name);
    }
    // A target named against a directory handle. Callers feed this straight to
    // `vpath_under_root`, which needs a full path, so join it here — and
    // decline when the parent is unknown rather than passing a bare leaf name
    // off as if it were absolute.
    let parent = parent_dir_of_handle(root_dir as HANDLE)?;
    let parent = parent.trim_end_matches(['\\', '/']);
    let rel = name.trim_start_matches(['\\', '/']);
    if rel.is_empty() {
        return Some(parent.to_string());
    }
    Some(format!("{parent}\\{rel}"))
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

    // DRM / identity exceptions (host Steam tree only — not Data/* content):
    // - steam_api*: SEC_IMAGE + client IPC
    // - steam_appid.txt: SteamAPI_Init / RestartAppIfNecessary
    // - SkyrimSE.exe / SkyrimSELauncher.exe: identity. Steam associates a
    //   process with an app by its *image path* vs the appmanifest installdir,
    //   and re-opens that path for version info / icon. Serving it through FUSE
    //   was observed to produce "Steam Error"; the cause is an open that fails
    //   to resolve (see tramp_create_abs and STATUS_OBJECT_NAME_NOT_FOUND on
    //   FUSE-relative OA), not an integrity check.
    //
    //   Steam does NOT compare the in-memory image against the on-disk PE.
    //   Measured while the launch still hollowed: the whole loaded image was
    //   overwritten with zip PE bytes at a relocated base and DRM verified
    //   fine. Do not "fix" anything here on the theory that the mapped image
    //   must match disk.
    {
        let base = std::path::Path::new(&path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if base.eq_ignore_ascii_case("steam_appid.txt")
            || base.eq_ignore_ascii_case("SkyrimSELauncher.exe")
        {
            return None; // tramp → the staged image's own directory
        }
        // steam_api* follows the same policy vfs-inject uses: when no copy is
        // staged beside the image, trampling to disk would just fail, so serve
        // it from the director instead.
        if (base.eq_ignore_ascii_case("steam_api64.dll")
            || base.eq_ignore_ascii_case("steam_api.dll"))
            && keep_host_steam_api()
        {
            return None;
        }
        // SkyrimSE.exe is excepted the same way by default, but is the one we
        // are still trying to explain — trace every open so the log shows who
        // asks for it and how (FUSE-relative OA vs absolute).
        if base.eq_ignore_ascii_case("SkyrimSE.exe") {
            let via_director = fuse_skyrim_exe();
            drm_exe_trace(&path, fuse_root_directory(oa), write, via_director);
            if !via_director {
                return None;
            }
        }
    }

    // All other under-root *reads* go through the director (zip / composed).
    // Writes may fall through for shim-local overlay redirect.
    // (Primary stack is expanded to 16 MiB by vfs-inject; open is a shallow ring op.)
    let opened = if write { client.open_write(vp) } else { client.open(vp) };
    match opened {
        Ok(resp) => {
            // Record absolute path on the handle so later relative opens
            // (RootDirectory=this handle) resolve through the director.
            let h = crate::fuse_synth::open_fuse_at(
                resp.fh,
                resp.size,
                resp.is_dir,
                Some(path.clone()),
            )?;
            if !file_handle.is_null() {
                *file_handle = h as HANDLE;
            }
            if !iosb.is_null() {
                let p = iosb as *mut u8;
                core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
                core::ptr::write_unaligned(p.add(8) as *mut usize, crate::ntdef::FILE_OPENED);
            }
            // Direct PATH_TABLE insert with absolute path (path_of may be relative OA).
            if let Ok(mut t) = PATH_TABLE.lock() {
                t.insert(h, path.clone());
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

/// Trace every `SkyrimSE.exe` open so the DRM exception can be explained rather
/// than assumed. Set `VFS_DRM_EXE_LOG` to a file path.
///
/// `rel` marks a FUSE-relative OA (RootDirectory is a synthetic handle), which
/// is the shape that previously failed with `STATUS_OBJECT_NAME_NOT_FOUND`.
fn drm_exe_trace(nt_or_win_path: &str, rel: bool, write: bool, via_director: bool) {
    let Some(path) = vfs_env::path(vfs_env::DRM_EXE_LOG) else {
        return;
    };
    let p = crate::fuse_client::strip_nt_device(nt_or_win_path.trim()).replace('/', "\\");
    let line = format!(
        "{}\tskyrimse-exe\troute={}\toa={}\taccess={}\t{}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        if via_director { "director" } else { "host" },
        if rel { "fuse-relative" } else { "absolute" },
        if write { "write" } else { "read" },
        p
    );
    if !hook_reenter_begin() {
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
    hook_reenter_end();
}

/// Optional proof that opens went through the director (not host disk).
/// Set `VFS_DIRECTOR_OPEN_LOG` to a file path.
fn director_open_trace(nt_or_win_path: &str, size: u64) {
    let Some(path) = vfs_env::path(vfs_env::DIRECTOR_OPEN_LOG) else {
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
    let mut _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Create);
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
    match path_of(oa) {
        Some(p) => crate::hookstats::note_passthrough(&p),
        // An open we cannot decode is an open we cannot serve. If the masters
        // are hiding anywhere, it is here.
        None => crate::hookstats::note_undecodable(oa_name_only(oa).as_deref()),
    }
    if let Some(st) = try_fuse_create(file_handle, oa, iosb, is_write_open(access, disp)) {
        if crate::hookstats::enabled() {
            if let Some(p) = path_of(oa) {
                crate::hookstats::note_trace("open", &p, if st >= 0 { "ok" } else { "FAIL" });
            }
        }
        _hs.mark_rooted();
        // FILE_SYNCHRONOUS_IO_ALERT | FILE_SYNCHRONOUS_IO_NONALERT. Absent means
        // the caller intends asynchronous completion, which a synthetic handle
        // cannot deliver by APC or completion port.
        crate::hookstats::note_open_sync(opts & 0x0000_0030 != 0);
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
            // Never pass a FUSE RootDirectory to the kernel (invalid handle).
            // DRM host exceptions resolve via path_of → absolute tramp instead.
            if fuse_root_directory(oa) {
                if let Some(path) = path_of(oa) {
                    let status = tramp_create_abs(
                        tramp, file_handle, access, oa, iosb, alloc, attrs, share, disp, opts,
                        ea, ealen, &path,
                    );
                    tag_under_root(file_handle, oa, status);
                    record_path(file_handle, oa, status);
                    return status;
                }
                return STATUS_OBJECT_NAME_NOT_FOUND;
            }
            let status =
                tramp(file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen);
            tag_under_root(file_handle, oa, status);
            record_path(file_handle, oa, status);
            status
        }
    }
}

/// True when OA.RootDirectory is a FUSE synthetic handle.
unsafe fn fuse_root_directory(oa: *const ObjectAttributes) -> bool {
    if oa.is_null() {
        return false;
    }
    let root = (*oa).root_directory;
    !root.is_null() && crate::fuse_synth::is_fuse_synth(root as isize)
}

/// Absolute `\??\` NT path for a Win32 or NT path string.
fn to_nt_path(path: &str) -> String {
    let p = crate::fuse_client::strip_nt_device(path.trim());
    if p.starts_with(r"\??\") {
        p.to_string()
    } else {
        format!(r"\??\{p}")
    }
}

/// Open via trampoline with an absolute NT path and **null** RootDirectory.
///
/// Required when the original OA had a FUSE synthetic RootDirectory (invalid
/// to the kernel) but we intentionally fall through to the host install —
/// DRM exceptions (`steam_api*`, `steam_appid.txt`, `SkyrimSE.exe`).
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
    let mut wbuf: Vec<u16> = nt.encode_utf16().collect();
    wbuf.push(0);
    let byte_len = ((wbuf.len() - 1) * 2) as u16;
    let new_us = UnicodeString {
        length: byte_len,
        maximum_length: byte_len + 2,
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
    let mut wbuf: Vec<u16> = nt.encode_utf16().collect();
    wbuf.push(0);
    let byte_len = ((wbuf.len() - 1) * 2) as u16;
    let new_us = UnicodeString {
        length: byte_len,
        maximum_length: byte_len + 2,
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
    let mut _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Open);
    let tramp = match TRAMP_OPEN {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if in_hook_reenter() {
        return tramp(file_handle, access, oa, iosb, share, opts);
    }
    match path_of(oa) {
        Some(p) => crate::hookstats::note_passthrough(&p),
        None => crate::hookstats::note_undecodable(oa_name_only(oa).as_deref()),
    }
    // NtOpenFile has no disposition — it always opens existing (FILE_OPEN). Pass
    // FILE_OPEN (1), NOT 0: 0 is FILE_SUPERSEDE, which is in is_write_open's
    // create/overwrite set and would misclassify every open as a write.
    if let Some(st) = try_fuse_create(file_handle, oa, iosb, is_write_open(access, vfs_redirect::FILE_OPEN)) {
        if crate::hookstats::enabled() {
            if let Some(p) = path_of(oa) {
                crate::hookstats::note_trace("open", &p, if st >= 0 { "ok" } else { "FAIL" });
            }
        }
        _hs.mark_rooted();
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
            if fuse_root_directory(oa) {
                if let Some(path) = path_of(oa) {
                    let status =
                        tramp_open_abs(tramp, file_handle, access, oa, iosb, share, opts, &path);
                    tag_under_root(file_handle, oa, status);
                    record_path(file_handle, oa, status);
                    return status;
                }
                return STATUS_OBJECT_NAME_NOT_FOUND;
            }
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

/// Stat-by-path, with no handle anywhere in the call.
///
/// Windows 11 routes existence checks here (class 77,
/// `FileStatBasicInformation`) instead of `NtQueryFullAttributesFile`, so an
/// unhooked build answers them from the real directory behind the mount. That
/// is silent by construction: the caller never opens anything, so nothing
/// appears in any open-side counter, and a game that tolerates a missing file
/// simply skips it. Skyrim's intro video and its master plugins both vanished
/// this way.
///
/// Only the classes that are pure metadata are filled. Anything else under the
/// root falls through, which is no worse than before this hook existed.
unsafe fn fill_by_name(
    class_raw: u32,
    info: *mut c_void,
    length: u32,
    is_dir: bool,
    size: u64,
) -> Option<usize> {
    let attrs = if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
    // Byte layouts per FILE_INFORMATION_CLASS. Written field-by-field with
    // unaligned writes because the caller's buffer has no alignment guarantee.
    let need: usize = match class_raw {
        4 => 40,   // FileBasicInformation
        5 => 24,   // FileStandardInformation
        34 => 56,  // FileNetworkOpenInformation
        68 => 72,  // FileStatInformation
        77 => 104, // FileStatBasicInformation (Win11)
        _ => return None,
    };
    if info.is_null() || (length as usize) < need {
        return None;
    }
    let p = info as *mut u8;
    core::ptr::write_bytes(p, 0, need);
    match class_raw {
        4 => {
            // 4x LARGE_INTEGER times, then FileAttributes.
            core::ptr::write_unaligned(p.add(32) as *mut u32, attrs);
        }
        5 => {
            core::ptr::write_unaligned(p as *mut i64, size as i64); // AllocationSize
            core::ptr::write_unaligned(p.add(8) as *mut i64, size as i64); // EndOfFile
            core::ptr::write_unaligned(p.add(16) as *mut u32, 1); // NumberOfLinks
            core::ptr::write_unaligned(p.add(21) as *mut u8, u8::from(is_dir)); // Directory
        }
        34 => {
            core::ptr::write_unaligned(p.add(32) as *mut i64, size as i64); // AllocationSize
            core::ptr::write_unaligned(p.add(40) as *mut i64, size as i64); // EndOfFile
            core::ptr::write_unaligned(p.add(48) as *mut u32, attrs);
        }
        68 | 77 => {
            // Both begin FileId, 4x time, AllocationSize, EndOfFile,
            // FileAttributes, ReparseTag, NumberOfLinks.
            core::ptr::write_unaligned(p.add(40) as *mut i64, size as i64); // AllocationSize
            core::ptr::write_unaligned(p.add(48) as *mut i64, size as i64); // EndOfFile
            core::ptr::write_unaligned(p.add(56) as *mut u32, attrs);
            core::ptr::write_unaligned(p.add(64) as *mut u32, 1); // NumberOfLinks
        }
        _ => return None,
    }
    Some(need)
}

unsafe extern "system" fn qibn_hook(
    oa: *const ObjectAttributes,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class_raw: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QByName);
    let tramp = match TRAMP_QIBN {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if in_hook_reenter() {
        return tramp(oa, iosb, info, length, class_raw);
    }
    if let Some(path) = path_of(oa) {
        let fuse = fuse_path_attr(&path);
        if fuse.is_none() {
            // Logged too: a stat that lands outside the root is exactly how a
            // wrong Data directory would present, and it is otherwise silent.
            crate::hookstats::note_stat(&path, &format!("byname{class_raw}-outside"));
        }
        if let Some(res) = fuse {
            match res {
                Ok((is_dir, size, _mtime)) => {
                    if let Some(n) = fill_by_name(class_raw, info, length, is_dir, size) {
                        crate::hookstats::note_stat(&path, &format!("byname{class_raw}-ok"));
                        if !iosb.is_null() {
                            let q = iosb as *mut u8;
                            core::ptr::write_unaligned(q as *mut u32, STATUS_SUCCESS as u32);
                            core::ptr::write_unaligned(q.add(8) as *mut usize, n);
                        }
                        return STATUS_SUCCESS;
                    }
                    crate::hookstats::note_stat(&path, &format!("byname{class_raw}-UNSUP"));
                }
                Err(st) if st == vfs_protocol::ST_NOT_FOUND => {
                    crate::hookstats::note_stat(&path, &format!("byname{class_raw}-missing"));
                    if !allow_disk_fallthrough() {
                        return STATUS_OBJECT_NAME_NOT_FOUND;
                    }
                }
                Err(_) => return STATUS_UNSUCCESSFUL,
            }
        }
        // The snapshot engine answers when no director is attached. Every other
        // metadata hook consults both sources; consulting only one here would
        // make this API disagree with `NtQueryAttributesFile` about whether the
        // very same file exists.
        if let Some(engine) = ENGINE.get() {
            match engine.query_attributes(&path) {
                AttrDecision::Attributes { is_dir, size, .. } => {
                    if let Some(n) = fill_by_name(class_raw, info, length, is_dir, size) {
                        if !iosb.is_null() {
                            let q = iosb as *mut u8;
                            core::ptr::write_unaligned(q as *mut u32, STATUS_SUCCESS as u32);
                            core::ptr::write_unaligned(q.add(8) as *mut usize, n);
                        }
                        return STATUS_SUCCESS;
                    }
                }
                AttrDecision::Deny => return STATUS_OBJECT_NAME_NOT_FOUND,
                AttrDecision::PassThrough => {}
            }
        }
    }
    tramp(oa, iosb, info, length, class_raw)
}

unsafe extern "system" fn qattr_hook(
    oa: *const ObjectAttributes,
    info: *mut FileBasicInformation,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QAttr);
    let tramp = match TRAMP_QATTR {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if in_hook_reenter() {
        return tramp(oa, info);
    }
    if let Some(path) = path_of(oa) {
        // Under-root: director only (zip/overrides). Never host Steam metadata.
        let fuse = fuse_path_attr(&path);
        if fuse.is_none() {
            crate::hookstats::note_stat(&path, "outside-root");
        }
        if let Some(res) = fuse {
            crate::hookstats::note_stat(
                &path,
                match &res {
                    Ok(_) => "found",
                    Err(st) if *st == vfs_protocol::ST_NOT_FOUND => "NOT-FOUND",
                    Err(_) => "ERROR",
                },
            );
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
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QFull);
    let tramp = match TRAMP_QFULL {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if in_hook_reenter() {
        return tramp(oa, info);
    }
    if let Some(path) = path_of(oa) {
        let fuse = fuse_path_attr(&path);
        if fuse.is_none() {
            crate::hookstats::note_stat(&path, "outside-root");
        }
        if let Some(res) = fuse {
            crate::hookstats::note_stat(
                &path,
                match &res {
                    Ok(_) => "found",
                    Err(st) if *st == vfs_protocol::ST_NOT_FOUND => "NOT-FOUND",
                    Err(_) => "ERROR",
                },
            );
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
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Close);
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
        // Releasing shim-owned VA waits for the last view (NT semantics).
        if let Some(window) = crate::zipserve::close_section(handle as isize) {
            crate::lazy_section::on_section_closed(window);
        }
        return STATUS_SUCCESS;
    }
    if let Ok(mut table) = DIR_TABLE.lock() {
        table.remove(&(handle as isize));
    }
    if let Ok(mut t) = HANDLE_PATHS.lock() {
        t.remove(&(handle as isize));
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
/// `FileCompletionInformation` — binds a handle to an I/O completion port.
const FILE_COMPLETION_INFORMATION: u32 = 30;

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
        if class == FILE_COMPLETION_INFORMATION {
            // Binding a synthetic handle to a completion port: the kernel will
            // never post a packet for it, so any caller waiting on that port
            // for this handle waits forever.
            crate::hookstats::note_iocp_bind();
        }
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
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QueryInfo);
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
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Write);
    let tramp = match TRAMP_WRITE {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        crate::hookstats::note_read_completion(!apc.is_null(), !event.is_null());
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
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Read);
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

/// Map a FUSE synthetic file into a synthetic section.
///
/// - **SEC_IMAGE**: map PE from director bytes (rare; PEs usually host-tramped).
/// - **Data ≤256 MiB**: eager stream into a private mapping (primary stack is
///   expanded to 16 MiB by vfs-inject — matches the known-good director-only path).
/// - **Data >256 MiB**: lazy demand-page (reserve + warm + VEH) so multi‑GiB BSAs
///   never full-preload.
unsafe fn fuse_create_section(
    section_handle: *mut HANDLE,
    max_size: *mut i64,
    _page_prot: u32,
    alloc_attrs: u32,
    file_handle: HANDLE,
) -> NTSTATUS {
    let Some((fh, size, is_dir, _)) = crate::fuse_synth::lookup(file_handle as isize) else {
        return STATUS_INVALID_HANDLE;
    };
    if is_dir || size == 0 {
        return STATUS_INVALID_FILE_FOR_SECTION;
    }
    // SEC_IMAGE: map PE image from director bytes.
    if alloc_attrs & SEC_IMAGE != 0 {
        if size > 256 * 1024 * 1024 {
            return STATUS_INVALID_FILE_FOR_SECTION;
        }
        let Some(client) = crate::fuse_client::global() else {
            return STATUS_UNSUCCESSFUL;
        };
        let mut pe = vec![0u8; size as usize];
        match client.read_fragmented(fh, 0, &mut pe) {
            Ok(n) if n == pe.len() => {}
            Ok(n) if n > 0 => pe.truncate(n),
            _ => return STATUS_INVALID_FILE_FOR_SECTION,
        }
        if !vfs_inject::pe_looks_like_image(&pe) {
            return STATUS_INVALID_FILE_FOR_SECTION;
        }
        return match vfs_inject::map_image_from_pe_bytes_local(&pe) {
            Ok((base, img_size)) => match crate::zipserve::register_mapped_image(base as usize, img_size as u64)
            {
                Some(h) => {
                    if !section_handle.is_null() {
                        *section_handle = h as HANDLE;
                    }
                    STATUS_SUCCESS
                }
                None => STATUS_INVALID_FILE_FOR_SECTION,
            },
            Err(_) => STATUS_INVALID_FILE_FOR_SECTION,
        };
    }
    if !max_size.is_null() {
        let want = core::ptr::read_unaligned(max_size);
        if want > 0 && (want as u64) > size {
            return STATUS_SECTION_TOO_BIG;
        }
    }
    // Diagnostic: `VFS_REJECT_FUSE_DATA_SECTION=1` refuses *data* sections only,
    // so the game falls back to ReadFile for content while SEC_IMAGE (DLL
    // loading) keeps working. Rejecting every section — the older
    // VFS_REJECT_FUSE_SECTION — breaks the launch outright.
    //
    // This is the one I/O path nothing else can observe: reads from a mapped
    // view are page faults served by the lazy-section VEH, so they appear in
    // neither NtReadFile nor the hook counters. Bypassing it makes that traffic
    // visible as ordinary reads.
    if vfs_env::present(vfs_env::REJECT_FUSE_DATA_SECTION) {
        return STATUS_INVALID_FILE_FOR_SECTION;
    }

    const EAGER_MAX: u64 = 256 * 1024 * 1024;
    if size > crate::lazy_section::MAX_LAZY {
        return STATUS_SECTION_TOO_BIG;
    }
    if size > EAGER_MAX {
        return match crate::lazy_section::create_lazy_data_section(fh, size) {
            Some(h) => {
                if !section_handle.is_null() {
                    *section_handle = h as HANDLE;
                }
                STATUS_SUCCESS
            }
            None => STATUS_SECTION_TOO_BIG,
        };
    }
    // Eager path (≤256 MiB): stream on this thread into VirtualAlloc.
    // Known-good with expand_primary_stack — avoid CreateThread from NtCreateSection.
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
    let fill_ok = match client.read_fragmented(fh, 0, dest) {
        Ok(n) if n == map_len => true,
        Ok(n) if n > 0 => {
            dest[n..].fill(0);
            true
        }
        _ => false,
    };
    // Opt-in trace only: this runs inside NtCreateSection, so the file I/O
    // re-enters our own hooks on every section the game creates.
    if let Some(path) = vfs_env::raw(vfs_env::SECTION_FILL_LOG) {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            use std::io::Write;
            let _ = writeln!(f, "eager fh={fh} size={size} ok={fill_ok}");
        }
    }
    if !fill_ok {
        VirtualFree(base, 0, MEM_RELEASE);
        return STATUS_UNSUCCESSFUL;
    }
    // Track the allocation so NtClose frees it — otherwise every eager section
    // leaks up to EAGER_MAX for the life of the process.
    crate::lazy_section::track_eager_section(base as usize, size);
    match crate::zipserve::register_mapped_image(base as usize, size) {
        Some(h) => {
            if !section_handle.is_null() {
                *section_handle = h as HANDLE;
            }
            STATUS_SUCCESS
        }
        None => {
            // Reaps the tracked region (no view, no open section) — which frees
            // `base`, so do not VirtualFree it again here.
            crate::lazy_section::on_section_closed(base as usize);
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
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::CreateSection);
    let tramp = match TRAMP_CREATE_SECTION {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // FUSE synthetic file handles: lazy data section or eager SEC_IMAGE.
    // Without this, NtCreateSection fails on fake handles (game mmap of BSAs).
    if crate::fuse_synth::is_fuse_synth(file_handle as isize) {
        // Debug: VFS_REJECT_FUSE_SECTION=1 forces ReadFile path (no section map).
        if vfs_env::present(vfs_env::REJECT_FUSE_SECTION) {
            return STATUS_INVALID_FILE_FOR_SECTION;
        }
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
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::MapView);
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
        let b = base as usize;
        // Retire one reference; the backing VA outlives it unless the section
        // handle is already closed and this was the last view — a BSA reader
        // slides views over one open section and must keep the others.
        crate::zipserve::unmap_view(b);
        crate::lazy_section::on_view_unmapped(b);
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

    // Start managed children in the virtual root, not the launcher's directory
    // (see `child_cwd_root`). Kept alive for the whole call: `cur_dir_eff` may
    // point into it.
    let root_cwd_w: Option<Vec<u16>> = if child_cwd_root() {
        vfs_env::text(vfs_env::VIRTUAL_DIR)
            .filter(|d| !d.is_empty())
            .map(|d| d.encode_utf16().chain(core::iter::once(0)).collect())
    } else {
        None
    };
    let cur_dir_eff: *const u16 = match &root_cwd_w {
        Some(v) => v.as_ptr(),
        None => cur_dir,
    };


    let forced = flags | CREATE_SUSPENDED;
    let r = tramp(
        token, app, cmd, proc_attr, thread_attr, inherit, forced, env, cur_dir_eff, si, pi, ptok,
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
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QDirEx);
    let tramp = match TRAMP_QDIREX {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    serve_dir_query(
        handle,
        iosb,
        info,
        length,
        class_raw,
        flags & SL_RESTART_SCAN != 0,
        flags & SL_RETURN_SINGLE_ENTRY != 0,
        file_name,
        &|| tramp(handle, event, apc, apc_ctx, iosb, info, length, class_raw, flags, file_name),
        &|h| drain_real(h, tramp),
    )
}

/// The classic entry point. Same body, different argument shape.
#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn qdir_hook(
    handle: HANDLE,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class_raw: u32,
    single: u8,
    file_name: *const UnicodeString,
    restart: u8,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QDir);
    let tramp = match TRAMP_QDIR {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // Draining still goes through the Ex trampoline: it is the same directory
    // and the Ex form is what `drain_real` speaks. Fall back to the classic
    // trampoline if Ex was never resolved.
    serve_dir_query(
        handle,
        iosb,
        info,
        length,
        class_raw,
        restart != 0,
        single != 0,
        file_name,
        &|| {
            tramp(
                handle, event, apc, apc_ctx, iosb, info, length, class_raw, single, file_name,
                restart,
            )
        },
        &|h| match TRAMP_QDIREX {
            Some(ex) => drain_real(h, ex),
            None => drain_real_classic(h, tramp),
        },
    )
}

/// `drain_real` for the classic trampoline, used only when `Ex` is unavailable.
unsafe fn drain_real_classic(handle: HANDLE, tramp: NtQueryDirectoryFileFn) -> Vec<DirItem> {
    const CLASS_FULL_DIR: u32 = 2;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut first = true;
    loop {
        let mut local_iosb = [0u8; 16];
        let restart = if first { 1u8 } else { 0u8 };
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
            0,
            core::ptr::null(),
            restart,
        );
        if st < 0 {
            break;
        }
        out.extend(parse_full_dir_info(&buf));
    }
    out
}

/// Shared body for both enumeration entry points.
#[allow(clippy::too_many_arguments)]
unsafe fn serve_dir_query(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class_raw: u32,
    restart: bool,
    single: bool,
    file_name: *const UnicodeString,
    passthrough: &dyn Fn() -> NTSTATUS,
    drain: &dyn Fn(HANDLE) -> Vec<DirItem>,
) -> NTSTATUS {
    // Unknown info class -> let the OS handle it verbatim.
    let class = match DirInfoClass::from_u32(class_raw) {
        Some(c) => c,
        None => return passthrough(),
    };
    let key = handle as isize;

    // Phase 1 (locked): is this a tracked handle, and must we (re)build?
    let (need_build, dir_path) = {
        let table = match DIR_TABLE.lock() {
            Ok(t) => t,
            Err(_) => return passthrough(),
        };
        match table.get(&key) {
            None => {
                drop(table);
                // Untracked: a directory outside the managed root, so the OS
                // answers. Worth recording anyway — "the game listed a Data
                // that isn't ours" is the diagnosis for an empty load order.
                if crate::hookstats::enabled() {
                    let dir = path_of_handle(handle).unwrap_or_else(|| "<unknown>".to_string());
                    crate::hookstats::note_readdir(
                        &dir,
                        wildcard_of(file_name).as_deref(),
                        0,
                        false,
                    );
                }
                return passthrough();
            }
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
                let real = drain(handle);
                Some(match ENGINE.get() {
                    Some(engine) => engine.merge_directory(&dir_path, &real, wildcard.as_deref()),
                    None => real,
                })
            }
        } else {
            let real = drain(handle);
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
        crate::hookstats::note_readdir(
            &dir_path,
            wildcard_of(file_name).as_deref(),
            merged.len(),
            true,
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Offsets and sizes of the metadata classes we answer by path.
    ///
    /// These are ABI, not our choice: the caller allocated the buffer and reads
    /// the fields at fixed offsets. Writing `EndOfFile` at the wrong offset does
    /// not fail — it reports a file of the wrong size, or a size of zero, which
    /// a caller is free to treat as "not worth opening". That is silent, so it
    /// gets pinned down here.
    const CLASS_BASIC: u32 = 4;
    const CLASS_STANDARD: u32 = 5;
    const CLASS_NETWORK_OPEN: u32 = 34;
    const CLASS_STAT: u32 = 68;
    const CLASS_STAT_BASIC: u32 = 77;

    fn fill(class: u32, buf: &mut [u8], is_dir: bool, size: u64) -> Option<usize> {
        unsafe {
            fill_by_name(
                class,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                is_dir,
                size,
            )
        }
    }

    fn u32_at(buf: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
    }

    fn i64_at(buf: &[u8], off: usize) -> i64 {
        i64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
    }

    #[test]
    fn every_supported_class_reports_its_documented_length() {
        for (class, want) in [
            (CLASS_BASIC, 40usize),
            (CLASS_STANDARD, 24),
            (CLASS_NETWORK_OPEN, 56),
            (CLASS_STAT, 72),
            (CLASS_STAT_BASIC, 104),
        ] {
            let mut buf = vec![0u8; want];
            assert_eq!(fill(class, &mut buf, false, 1), Some(want), "class {class}");
        }
    }

    /// A short buffer must be declined, not partially written: the caller sized
    /// it for a different class and every byte past its end belongs to someone.
    #[test]
    fn a_buffer_one_byte_short_is_refused() {
        for (class, need) in [
            (CLASS_BASIC, 40usize),
            (CLASS_STANDARD, 24),
            (CLASS_NETWORK_OPEN, 56),
            (CLASS_STAT, 72),
            (CLASS_STAT_BASIC, 104),
        ] {
            let mut buf = vec![0xAAu8; need - 1];
            assert_eq!(fill(class, &mut buf, false, 1), None, "class {class}");
            assert!(buf.iter().all(|b| *b == 0xAA), "class {class} wrote into a short buffer");
        }
    }

    #[test]
    fn an_unknown_class_is_declined_so_the_caller_falls_through() {
        let mut buf = vec![0u8; 512];
        assert_eq!(fill(9999, &mut buf, false, 1), None);
    }

    /// The size a stat reports is the whole reason these classes are answered:
    /// a caller that sees zero bytes may skip the file without ever opening it.
    #[test]
    fn size_lands_at_the_offset_each_class_defines() {
        const SIZE: u64 = 249_753_412; // Skyrim.esm, i.e. well past 32 bits.
        let mut buf = vec![0u8; 104];

        fill(CLASS_STANDARD, &mut buf, false, SIZE).unwrap();
        assert_eq!(i64_at(&buf, 0), SIZE as i64, "standard AllocationSize");
        assert_eq!(i64_at(&buf, 8), SIZE as i64, "standard EndOfFile");

        buf.iter_mut().for_each(|b| *b = 0);
        fill(CLASS_NETWORK_OPEN, &mut buf, false, SIZE).unwrap();
        assert_eq!(i64_at(&buf, 40), SIZE as i64, "network-open EndOfFile");

        for class in [CLASS_STAT, CLASS_STAT_BASIC] {
            buf.iter_mut().for_each(|b| *b = 0);
            fill(class, &mut buf, false, SIZE).unwrap();
            assert_eq!(i64_at(&buf, 40), SIZE as i64, "class {class} AllocationSize");
            assert_eq!(i64_at(&buf, 48), SIZE as i64, "class {class} EndOfFile");
        }
    }

    #[test]
    fn directories_are_distinguishable_from_files_in_every_class() {
        let mut buf = vec![0u8; 104];

        for (class, attr_off) in [
            (CLASS_BASIC, 32usize),
            (CLASS_NETWORK_OPEN, 48),
            (CLASS_STAT, 56),
            (CLASS_STAT_BASIC, 56),
        ] {
            buf.iter_mut().for_each(|b| *b = 0);
            fill(class, &mut buf, true, 0).unwrap();
            assert_eq!(
                u32_at(&buf, attr_off) & FILE_ATTRIBUTE_DIRECTORY,
                FILE_ATTRIBUTE_DIRECTORY,
                "class {class} did not mark a directory"
            );

            buf.iter_mut().for_each(|b| *b = 0);
            fill(class, &mut buf, false, 1).unwrap();
            assert_eq!(
                u32_at(&buf, attr_off) & FILE_ATTRIBUTE_DIRECTORY,
                0,
                "class {class} marked a file as a directory"
            );
        }

        // FileStandardInformation carries a boolean rather than an attribute.
        buf.iter_mut().for_each(|b| *b = 0);
        fill(CLASS_STANDARD, &mut buf, true, 0).unwrap();
        assert_eq!(buf[21], 1, "standard Directory flag");
        buf.iter_mut().for_each(|b| *b = 0);
        fill(CLASS_STANDARD, &mut buf, false, 1).unwrap();
        assert_eq!(buf[21], 0, "standard Directory flag set for a file");
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
        assert_eq!(parse_rename(&mut buf).as_deref(), Some(r"\??\C:\root\new.esp"));
    }

    /// A rename target may be named against a directory handle. Refusing to
    /// decode those is the same defect that made relative *opens* invisible —
    /// the rename would fall through unvirtualised and hit the real directory.
    #[test]
    fn a_rename_target_relative_to_a_known_handle_becomes_a_full_path() {
        let handle = 0x4321usize;
        HANDLE_PATHS
            .lock()
            .unwrap()
            .insert(handle as isize, r"\??\C:\root\Data".to_string());

        let mut buf = rename_info(handle, "new.esp");
        assert_eq!(
            parse_rename(&mut buf).as_deref(),
            Some(r"\??\C:\root\Data\new.esp"),
            "a handle-relative target must be joined to its parent"
        );

        HANDLE_PATHS.lock().unwrap().remove(&(handle as isize));
    }

    /// An unknown parent must yield nothing. Returning the bare leaf would be
    /// worse than declining: callers treat the result as a full path.
    #[test]
    fn a_rename_target_with_an_unknown_parent_is_declined() {
        let mut buf = rename_info(0xDEAD_BEEF, "new.esp");
        assert_eq!(parse_rename(&mut buf), None);
    }
}
