//! The ntdll detours. ALL `unsafe` in the crate lives here.
#![allow(unsafe_code)]

use core::cell::Cell;
use core::ffi::c_void;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

// Host-side metadata probes from inside hooks must not re-enter detours
// (`Path::is_file` → NtCreateFile → try_fuse_create → … → stack overflow).
thread_local! {
    static HOOK_REENTER: Cell<u32> = const { Cell::new(0) };
}

fn in_hook_reenter() -> bool {
    HOOK_REENTER.with(|c| c.get() > 0)
}

/// RAII form of [`HOOK_REENTER`] for shim-initiated file I/O. `enter()` returns
/// `None` when the guard is already held on this thread — the caller's signal
/// that it is already running *inside* the shim's own I/O and must not start
/// more. While it is held, every NT file call this thread makes takes
/// `create_hook`/`open_hook`'s `in_hook_reenter` fast path straight to the real
/// ntdll — which is the point: copy-up writes its destination file while the
/// hook that asked for the copy-up is still on the stack.
///
/// **This is the only way to raise the counter, and that is deliberate rather
/// than tidy.** There used to be a `hook_reenter_begin`/`hook_reenter_end`
/// pair as well, and three in-module callers used it raw: `install_panic_hook`,
/// `drm_exe_trace` and `director_open_trace`. Each of those does file I/O
/// between the two calls, and a panic anywhere in that span skipped the `end`.
/// That failure is permanent and completely silent: the counter stays at 1 for
/// the life of the thread, so every later hook call from it takes the
/// `in_hook_reenter` fast path to real ntdll, and the process quietly stops
/// being virtualized on that thread while every counter keeps reporting
/// ordinary activity. Now that `hook::contain_panic` catches panics instead of
/// letting them abort the process, that "later" actually exists — so the pair
/// is gone and the counter can only be raised by a value whose `Drop` lowers
/// it, unwinding included.
pub(crate) struct ShimIoGuard(());

impl ShimIoGuard {
    pub(crate) fn enter() -> Option<Self> {
        HOOK_REENTER.with(|c| {
            let d = c.get();
            if d > 0 {
                return None;
            }
            c.set(d + 1);
            Some(ShimIoGuard(()))
        })
    }
}

impl Drop for ShimIoGuard {
    fn drop(&mut self) {
        HOOK_REENTER.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// What every hook returns when [`contain_panic`] catches a panic in its body.
///
/// **The choice that matters is that it is a failure**, with the NTSTATUS
/// severity bits set, so no caller can mistake it for a completed operation. A
/// hook that panicked half way through has written nothing to the caller's
/// output buffer and stored nothing in its `*mut HANDLE`; answering
/// `STATUS_SUCCESS` would hand the game an uninitialised handle value and a
/// buffer of stack garbage to parse, which is materially worse than the abort
/// this replaces — a crash at least stops at the fault.
///
/// It is `STATUS_UNSUCCESSFUL` and not something more specific for the opposite
/// reason. A panic means the shim does not know what happened, and every
/// *specific* status is a claim it is not entitled to make: returning
/// `STATUS_OBJECT_NAME_NOT_FOUND` tells the game the file does not exist, and
/// Skyrim will happily bake that into a load order and carry on without the
/// plugin; `STATUS_ACCESS_DENIED` invites a retry loop; `STATUS_NO_MORE_FILES`
/// from an enumeration hook silently truncates a directory listing. Generic
/// failure is the only answer that says "this operation did not happen" without
/// also asserting why.
///
/// It is uniform across all twenty ntdll entry points because a panic is the
/// same event in each of them, and because a per-hook table of "best" statuses
/// would be twenty more chances to pick one that a caller treats as benign.
/// `cpiw_hook` is the one exception and is not an exception to the principle:
/// `CreateProcessInternalW` returns a Win32 `BOOL`, where this constant's bit
/// pattern is *non-zero* and therefore reads as success. It returns `FALSE`
/// instead — see the `on_panic` expression in the `hook_entry_points!` list.
///
/// Note that several hooks already return this same status when their
/// trampoline is missing, so the value alone does not distinguish a panic from
/// that. The distinguishing record is `hookstats::note_hook_panic` plus the
/// shim panic log, both of which a panic writes and a missing trampoline does
/// not.
const STATUS_HOOK_PANICKED: NTSTATUS = STATUS_UNSUCCESSFUL;

/// Run one hook body with its panic contained at the `extern "system"`
/// boundary, and report `on_panic`'s value to the caller if it faults.
///
/// **What this changes, and what it does not.** Measured on this toolchain
/// (2026-08-16, `rustc` with `panic = "unwind"`): a panic inside an
/// `extern "system"` fn runs the `Drop` impls of every Rust frame below the
/// boundary, in order, and *then* hits rustc's forced
/// `core::panicking::panic_cannot_unwind` and takes the process down with
/// `0xC0000409`. Adding this wrapper does not change which destructors run —
/// the same unwind runs the same ones — it changes only where the unwind stops
/// and what happens there. The unwind never reached the game's frames before
/// (the forced abort is at *our* boundary, not theirs) and still does not; what
/// the game gets now is a returned status instead of a dead process.
///
/// That the inner destructors run is a requirement here, not a tolerated cost:
/// [`ShimIoGuard`]'s `Drop` is what releases this thread's reentrancy counter,
/// and a panic that skipped it would leave every later hook call on that thread
/// falling through to real ntdll — a silent un-virtualization far harder to
/// diagnose than a crash.
///
/// # `AssertUnwindSafe`
///
/// Every hook body captures raw pointers from the NT call and touches process
/// statics, so none of these closures is `UnwindSafe` and the assertion is
/// unavoidable. It is also true here, for a reason narrower than the general
/// case: `UnwindSafe` guards against *observing* state that a caught unwind
/// left half-updated, and this function observes nothing. On the `Err` arm it
/// reads nothing out of the closure, touches none of the captured pointers, and
/// returns a constant. Every process-wide table this crate shares between hooks
/// (`DIR_TABLE`, `IDENTITY_TABLE`, `PATH_TABLE`, `HANDLE_PATHS`, and
/// `hookstats`' accumulators) is behind a `std::sync::Mutex`, which poisons on
/// a panic taken while held, and every lock site in this crate already treats
/// `Err` as "no entry". So a later call cannot read a torn value out of one; it
/// reads nothing, which is the same thing it does on any other lock failure.
///
/// The honest consequence of that, which the abort did not have because there
/// was no "later": a panic taken while one of those tables is locked poisons it
/// for the rest of the process, and the handle tracking it backs degrades to
/// permanently empty. That is a real loss of fidelity and it is why the caught
/// panic is counted loudly rather than swallowed.
///
/// # Visibility
///
/// `pub` because the shim's `extern "system"` surface is not confined to this
/// crate: `vfs-shim-dll` owns `DllMain` and `vfs_shim_sync_bootstrap`, which are
/// entry points Windows and the OEP stub call, and which must contain their
/// panics for the same reason and by the same route. One containment function for
/// the whole injected DLL is what lets
/// `no_extern_hook_bypasses_the_panic_containment_macro` check both crates
/// against a single marker.
pub fn contain_panic<R>(
    name: &'static str,
    body: impl FnOnce() -> R,
    on_panic: impl FnOnce() -> R,
) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(v) => v,
        Err(_) => {
            // The panic's message, location and thread were already written by
            // `install_panic_hook`, which runs before any unwinding. This adds
            // the aggregate the log cannot give: how many, and where.
            crate::hookstats::note_hook_panic(name);
            on_panic()
        }
    }
}

/// Generates the `extern "system"` entry point for each detour: a wrapper that
/// runs the real body inside [`contain_panic`].
///
/// Every hook goes through this one macro rather than each wrapping its own
/// body, so "does this entry point contain its panics" cannot be answered
/// differently for different hooks — and so a hook added later cannot quietly
/// skip it. `no_extern_hook_bypasses_the_panic_containment_macro` in this
/// module's tests enforces that by scanning the source: this macro must be the
/// only place in `hook.rs` that defines an `extern "system"` function.
macro_rules! hook_entry_points {
    ($(
        $(#[$attr:meta])*
        fn $wrapper:ident = $body:ident($($arg:ident: $ty:ty),* $(,)?) -> $ret:ty
            as $name:literal, on_panic $fallback:expr;
    )*) => {$(
        #[doc = concat!(
            "Panic-contained `extern \"system\"` entry point for `", $name,
            "`. The body is [`", stringify!($body), "`]; this wrapper exists so a \
             panic in it returns a failure status instead of aborting the game — \
             see [`contain_panic`]."
        )]
        $(#[$attr])*
        #[allow(clippy::too_many_arguments)]
        unsafe extern "system" fn $wrapper($($arg: $ty),*) -> $ret {
            contain_panic($name, || unsafe { $body($($arg),*) }, || $fallback)
        }
    )*};
}

hook_entry_points! {
    fn create_hook = create_hook_body(
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
    ) -> NTSTATUS as "NtCreateFile", on_panic STATUS_HOOK_PANICKED;

    fn open_hook = open_hook_body(
        file_handle: *mut HANDLE,
        access: u32,
        oa: *const ObjectAttributes,
        iosb: *mut c_void,
        share: u32,
        opts: u32,
    ) -> NTSTATUS as "NtOpenFile", on_panic STATUS_HOOK_PANICKED;

    fn qibn_hook = qibn_hook_body(
        oa: *const ObjectAttributes,
        iosb: *mut c_void,
        info: *mut c_void,
        length: u32,
        class_raw: u32,
    ) -> NTSTATUS as "NtQueryInformationByName", on_panic STATUS_HOOK_PANICKED;

    fn qattr_hook = qattr_hook_body(
        oa: *const ObjectAttributes,
        info: *mut FileBasicInformation,
    ) -> NTSTATUS as "NtQueryAttributesFile", on_panic STATUS_HOOK_PANICKED;

    fn qfull_hook = qfull_hook_body(
        oa: *const ObjectAttributes,
        info: *mut FileNetworkOpenInformation,
    ) -> NTSTATUS as "NtQueryFullAttributesFile", on_panic STATUS_HOOK_PANICKED;

    fn close_hook = close_hook_body(handle: HANDLE) -> NTSTATUS
        as "NtClose", on_panic STATUS_HOOK_PANICKED;

    fn delete_hook = delete_hook_body(oa: *const ObjectAttributes) -> NTSTATUS
        as "NtDeleteFile", on_panic STATUS_HOOK_PANICKED;

    fn qif_hook = qif_hook_body(
        handle: HANDLE,
        iosb: *mut c_void,
        info: *mut c_void,
        length: u32,
        class: u32,
    ) -> NTSTATUS as "NtQueryInformationFile", on_panic STATUS_HOOK_PANICKED;

    fn qobj_hook = qobj_hook_body(
        handle: HANDLE,
        class: u32,
        info: *mut c_void,
        length: u32,
        ret_len: *mut u32,
    ) -> NTSTATUS as "NtQueryObject", on_panic STATUS_HOOK_PANICKED;

    fn setinfo_hook = setinfo_hook_body(
        handle: HANDLE,
        iosb: *mut c_void,
        info: *mut c_void,
        length: u32,
        class: u32,
    ) -> NTSTATUS as "NtSetInformationFile", on_panic STATUS_HOOK_PANICKED;

    fn qvol_hook = qvol_hook_body(
        handle: HANDLE,
        iosb: *mut c_void,
        info: *mut c_void,
        length: u32,
        class: u32,
    ) -> NTSTATUS as "NtQueryVolumeInformationFile", on_panic STATUS_HOOK_PANICKED;

    fn lock_hook = lock_hook_body(
        handle: HANDLE,
        event: HANDLE,
        apc: *const c_void,
        apc_ctx: *const c_void,
        iosb: *mut c_void,
        byte_offset: *const i64,
        length: *const i64,
        key: u32,
        fail_immediately: u8,
        exclusive: u8,
    ) -> NTSTATUS as "NtLockFile", on_panic STATUS_HOOK_PANICKED;

    fn unlock_hook = unlock_hook_body(
        handle: HANDLE,
        iosb: *mut c_void,
        byte_offset: *const i64,
        length: *const i64,
        key: u32,
    ) -> NTSTATUS as "NtUnlockFile", on_panic STATUS_HOOK_PANICKED;

    fn flush_hook = flush_hook_body(handle: HANDLE, iosb: *mut c_void) -> NTSTATUS
        as "NtFlushBuffersFile", on_panic STATUS_HOOK_PANICKED;

    fn write_hook = write_hook_body(
        handle: HANDLE,
        event: HANDLE,
        apc: *const c_void,
        apc_ctx: *const c_void,
        iosb: *mut c_void,
        buffer: *mut c_void,
        length: u32,
        byte_offset: *const i64,
        key: *const u32,
    ) -> NTSTATUS as "NtWriteFile", on_panic STATUS_HOOK_PANICKED;

    fn read_hook = read_hook_body(
        handle: HANDLE,
        event: HANDLE,
        apc: *const c_void,
        apc_ctx: *const c_void,
        iosb: *mut c_void,
        buffer: *mut c_void,
        length: u32,
        byte_offset: *const i64,
        key: *const u32,
    ) -> NTSTATUS as "NtReadFile", on_panic STATUS_HOOK_PANICKED;

    fn create_section_hook = create_section_hook_body(
        section_handle: *mut HANDLE,
        access: u32,
        oa: *const ObjectAttributes,
        max_size: *mut i64,
        page_prot: u32,
        alloc_attrs: u32,
        file_handle: HANDLE,
    ) -> NTSTATUS as "NtCreateSection", on_panic STATUS_HOOK_PANICKED;

    fn map_view_hook = map_view_hook_body(
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
    ) -> NTSTATUS as "NtMapViewOfSection", on_panic STATUS_HOOK_PANICKED;

    fn unmap_view_hook = unmap_view_hook_body(process: HANDLE, base: *mut c_void) -> NTSTATUS
        as "NtUnmapViewOfSection", on_panic STATUS_HOOK_PANICKED;

    fn qdirex_hook = qdirex_hook_body(
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
    ) -> NTSTATUS as "NtQueryDirectoryFileEx", on_panic STATUS_HOOK_PANICKED;

    fn qdir_hook = qdir_hook_body(
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
    ) -> NTSTATUS as "NtQueryDirectoryFile", on_panic STATUS_HOOK_PANICKED;

    /// The one entry point here that is **not** an ntdll `NTSTATUS` call, and
    /// the one place a uniform `STATUS_UNSUCCESSFUL` would be actively
    /// dangerous. `CreateProcessInternalW` returns a Win32 `BOOL`, in which
    /// `STATUS_UNSUCCESSFUL`'s bit pattern is non-zero and therefore reads as
    /// **success** — the caller would then go on to use a `PROCESS_INFORMATION`
    /// nothing ever filled in, and close or wait on two garbage handles. `FALSE`
    /// is the failure value in this ABI, and `SetLastError` is part of the
    /// contract: a `BOOL`-returning Win32 function that fails without setting it
    /// leaves the caller reporting whatever error some unrelated earlier call
    /// happened to leave behind.
    fn cpiw_hook = cpiw_hook_body(
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
    ) -> i32 as "CreateProcessInternalW", on_panic {
        // SAFETY: plain TLS write in the current process; no pointers involved.
        unsafe { windows_sys::Win32::Foundation::SetLastError(ERROR_INTERNAL_ERROR) };
        0
    };
}

/// Opt-in only: when `VFS_ALLOW_DISK_FALLTHROUGH=1`, under-root FUSE NOT_FOUND
/// may open the host path (legacy / debug). Default **off** — game content must
/// come from the director (zip/overrides), never the Steam library tree.
fn allow_disk_fallthrough() -> bool {
    static FLAG: OnceLock<bool> = OnceLock::new();
    *FLAG.get_or_init(|| vfs_env::opt_in(vfs_env::ALLOW_DISK_FALLTHROUGH))
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

use retour::RawDetour;
use vfs_redirect::{
    nt_to_volume_relative, write_dir_info, write_file_name_info,
    Decision, DirInfoClass, DirItem, DirStatus,
};
use windows_sys::Win32::Foundation::{ERROR_INTERNAL_ERROR, HANDLE, HMODULE, NTSTATUS};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
use windows_sys::Win32::System::Threading::{
    ResumeThread, CREATE_SUSPENDED, PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::engine::Engine;
use crate::inject::{inject_child, re_suspend, self_dll_path};
use crate::overlay::OverlayState;
use crate::ntdef::{

    FileBasicInformation, FileFsDeviceInformation,
    FileEndOfFileInformation, FileInternalInformation, FileNetworkOpenInformation,
    FilePositionInformation,
    FileStandardInformation, NtCloseFn, NtCreateFileFn, NtCreateSectionFn,
    NtDeleteFileFn, NtFlushBuffersFileFn, NtLockFileFn, NtMapViewOfSectionFn,
    NtOpenFileFn, NtQueryAttributesFileFn, NtQueryDirectoryFileExFn, NtQueryDirectoryFileFn,
    NtQueryInformationByNameFn, NtQueryObjectFn, NtUnlockFileFn,
    NtQueryFullAttributesFileFn,
    NtQueryInformationFileFn, NtQueryVolumeInformationFileFn, NtReadFileFn, NtSetInformationFileFn,
    NtWriteFileFn, NtUnmapViewOfSectionFn, ObjectAttributes, UnicodeString, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ALL_INFORMATION,
    FILE_BASIC_INFORMATION, FILE_CREATED, FILE_DEVICE_DISK, FILE_DIRECTORY_FILE,
    FILE_DISPOSITION_DELETE, FILE_DISPOSITION_INFORMATION,
    FILE_DISPOSITION_INFORMATION_EX, FILE_END_OF_FILE_INFORMATION, FILE_FS_DEVICE_INFORMATION,
    FILE_INTERNAL_INFORMATION,
    FILE_NAME_INFORMATION,
    FILE_NETWORK_OPEN_INFORMATION, FILE_NORMALIZED_NAME_INFORMATION, FILE_POSITION_INFORMATION,
    FILE_RENAME_INFORMATION, FILE_RENAME_INFORMATION_EX, FILE_STANDARD_INFORMATION, SEC_IMAGE,
    OBJECT_NAME_INFORMATION, OBJECT_NAME_INFORMATION_HEADER,
    SL_RESTART_SCAN, SL_RETURN_SINGLE_ENTRY, STATUS_ACCESS_DENIED, STATUS_BUFFER_OVERFLOW,
    STATUS_INFO_LENGTH_MISMATCH,
    STATUS_END_OF_FILE, STATUS_INVALID_FILE_FOR_SECTION, STATUS_INVALID_HANDLE,
    STATUS_FILE_IS_A_DIRECTORY, STATUS_NO_MORE_FILES, STATUS_OBJECT_NAME_COLLISION,
    STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SECTION_TOO_BIG, STATUS_SUCCESS, STATUS_UNSUCCESSFUL,
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
static mut TRAMP_DELETE: Option<NtDeleteFileFn> = None;
static mut TRAMP_CLOSE: Option<NtCloseFn> = None;
static mut TRAMP_QIF: Option<NtQueryInformationFileFn> = None;
static mut TRAMP_QOBJ: Option<NtQueryObjectFn> = None;
static mut TRAMP_SETINFO: Option<NtSetInformationFileFn> = None;
static mut TRAMP_READ: Option<NtReadFileFn> = None;
static mut TRAMP_WRITE: Option<NtWriteFileFn> = None;
/// The timestamp reported for every VFS-backed file.
///
/// Not zero, and that is the whole point. A `FILETIME` of 0 is 1 January
/// 1601, and Cyberpunk 2077 refuses to start against it: it stats
/// `r6/cache/final.redscripts`, gets 1601, and puts up "encountered an error
/// caused by a corrupted or missing scripts file". Reporting a plausible date
/// instead takes it from that dialog to a running game window. Skyrim SE never
/// looked, which is why this survived until a second game existed.
///
/// The value is arbitrary but must be **stable across runs** and **not in the
/// future**. Stability matters more than accuracy: a timestamp that moved every
/// launch would invalidate exactly the caches this exists to satisfy, and a
/// game that recompiles its script cache on every boot is no better off than
/// one that refuses to boot.
///
/// Providers carry no real timestamps to plumb through instead — a Steam depot
/// manifest has none, so ocm's depot provider supplies `mtime: 0` honestly.
/// Should a provider ever have real times, they belong here in place of the
/// constant.
///
/// 2024-01-01T00:00:00Z, in 100 ns ticks since 1601.
const SYNTH_FILETIME: i64 = 133_485_408_000_000_000;

static mut TRAMP_CREATE_SECTION: Option<NtCreateSectionFn> = None;
static mut TRAMP_MAP_VIEW: Option<NtMapViewOfSectionFn> = None;
static mut TRAMP_UNMAP_VIEW: Option<NtUnmapViewOfSectionFn> = None;
static mut TRAMP_QVOL: Option<NtQueryVolumeInformationFileFn> = None;
static mut TRAMP_LOCK: Option<NtLockFileFn> = None;
static mut TRAMP_UNLOCK: Option<NtUnlockFileFn> = None;
static mut TRAMP_FLUSH: Option<NtFlushBuffersFileFn> = None;
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

/// Per-handle enumeration cursor over a built directory listing.
///
/// The field was `merged` when a listing really was a merge of the real
/// directory with a snapshot or overlay. Nothing merges any more: under a
/// managed root this is the director's own `readdir`, whole and unaltered
/// (see `serve_dir_query`), and a directory outside every root never gets an
/// `EnumState` at all — the OS answers it directly.
struct EnumState {
    entries: Vec<DirItem>,
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

/// Detours `install_all_detours` passed over because the host's ntdll does not
/// export the function. `BTreeSet::new()` is `const`, so this needs no lazy init.
///
/// Empty on Windows. Non-empty under Wine, whose ntdll omits the newer
/// enumeration and query entry points. This exists so a skipped hook is a fact a
/// host can read rather than an invisible gap: an unhooked handle-taking NT API
/// does not error, it quietly serves the real directory instead of the composed
/// one, which reads exactly like a mod list that is simply empty.
static SKIPPED_DETOURS: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());

fn note_skipped_detour(name: &'static str) {
    if let Ok(mut s) = SKIPPED_DETOURS.lock() {
        s.insert(name);
    }
}

/// Names of hooks not installed because ntdll had no such export.
///
/// Empty means every hook this build knows about is live. A caller that requires
/// total interception should treat a non-empty result as fatal rather than
/// advisory — see [`SKIPPED_DETOURS`].
pub fn skipped_detours() -> Vec<&'static str> {
    SKIPPED_DETOURS
        .lock()
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default()
}

/// Resolve `name` in ntdll and build (not yet enabled) a detour to `hookfn`.
unsafe fn make_detour(
    ntdll: HMODULE,
    name: &core::ffi::CStr,
    hookfn: *const (),
) -> Result<RawDetour, InstallError> {
    let proc = GetProcAddress(ntdll, name.as_ptr().cast()).ok_or(InstallError::ProcMissing)?;
    RawDetour::new(proc as *const (), hookfn).map_err(|_| InstallError::Detour)
}

/// Install all detours backed by `engine` (in-process / no early payload).
/// Idempotent-guarded. Patches the four path/attr stubs itself.
pub fn install(engine: Engine) -> Result<HookGuard, InstallError> {
    install_panic_hook();
    // Before the detours go live: creating the breadcrumb file is real I/O, and
    // once hooks are installed that I/O re-enters them.
    crate::breadcrumb::init();
    crate::hookstats::start_reporter();
    ENGINE.set(engine).map_err(|_| InstallError::AlreadyInstalled)?;
    // SAFETY: ntdll lookup + detour install; each hook matches its ABI.
    unsafe { install_all_detours(true) }
}

/// Record shim panics — which no longer take the game down, and that is the
/// change that matters most about this comment.
///
/// **A panic in a hook used to end the process, and does not any more.** The
/// history is worth keeping because two successive versions of this comment
/// were wrong about why. The first claimed the workspace builds with
/// `panic = "abort"`; it does not — `rust/Cargo.toml` sets `panic = "unwind"`
/// for both profiles, deliberately, so a future binding can turn a panic into
/// a host-language exception. The second, correct as far as it went, was that
/// the process died anyway because every hook is `extern "system"` and rustc
/// plants a forced abort wherever an unwind would cross that boundary:
/// measured, a panic inside such a function printed `thread caused
/// non-unwinding panic. aborting.` and exited **0xC0000409**
/// (`FAST_FAIL_FATAL_APP_EXIT`). That is no longer what happens, because
/// [`contain_panic`] now catches at each boundary and returns
/// [`STATUS_HOOK_PANICKED`] instead. The old behaviour is still one edit away
/// and reproducible on demand — deleting the `catch_unwind` makes
/// `a_panicking_hook_returns_a_failure_status_instead_of_aborting` kill the
/// *test process* with exactly that code.
///
/// So this hook's job changed from "attribute the crash" to "be the only
/// record there was a fault at all". It matters more now, not less: a
/// contained panic is invisible from outside the process — the game gets a
/// failed file operation and carries on — and this log is the only place the
/// message, location and thread survive. (`hookstats`' caught-panic counters
/// are the aggregate, but they only reach a reader if `VFS_SHIM_STATS_LOG` is
/// set.) The 0xC0000409 attribution still matters for the panics that *do*
/// abort — a panic inside this hook, or in code the containment does not
/// cover — where the exit is otherwise an unattributable
/// `STATUS_STACK_BUFFER_OVERRUN`, indistinguishable from a genuine
/// stack-cookie or CFG failure in the game, and localisable only by bisecting
/// (see the 0xC0000409 hunt behind commit 5f8f2eb).
///
/// `set_hook`'s hook runs at panic time, before any unwinding begins, so the
/// message is written whether the unwind is later caught or aborts. A logged
/// message therefore does **not** imply the process died, and now for two
/// reasons rather than one: a panic on the stats reporter thread kills only
/// that thread and never reaches an `extern` boundary at all, and a panic in a
/// hook is caught at that boundary and answered with a status.
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
            if let Some(_io) = ShimIoGuard::enter() {
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
    // Before the detours go live: creating the breadcrumb file is real I/O, and
    // once hooks are installed that I/O re-enters them.
    crate::breadcrumb::init();
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
    let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
    if ntdll.is_null() {
        return Err(InstallError::NtdllMissing);
    }

    let mut detours: Vec<RawDetour> = Vec::new();

    if patch_early_owned {
        let d_create = make_detour(ntdll, c"NtCreateFile", create_hook as *const ())?;
        TRAMP_CREATE = Some(core::mem::transmute::<*const (), NtCreateFileFn>(
            d_create.trampoline() as *const (),
        ));
        let d_qattr = make_detour(ntdll, c"NtQueryAttributesFile", qattr_hook as *const ())?;
        TRAMP_QATTR = Some(core::mem::transmute::<*const (), NtQueryAttributesFileFn>(
            d_qattr.trampoline() as *const (),
        ));
        let d_qfull =
            make_detour(ntdll, c"NtQueryFullAttributesFile", qfull_hook as *const ())?;
        TRAMP_QFULL = Some(core::mem::transmute::<*const (), NtQueryFullAttributesFileFn>(
            d_qfull.trampoline() as *const (),
        ));
        let d_open = make_detour(ntdll, c"NtOpenFile", open_hook as *const ())?;
        TRAMP_OPEN = Some(core::mem::transmute::<*const (), NtOpenFileFn>(
            d_open.trampoline() as *const (),
        ));
        d_create.enable().map_err(|_| InstallError::Detour)?;
        d_qattr.enable().map_err(|_| InstallError::Detour)?;
        d_qfull.enable().map_err(|_| InstallError::Detour)?;
        d_open.enable().map_err(|_| InstallError::Detour)?;
        detours.extend([d_create, d_qattr, d_qfull, d_open]);
    }

    // Present since Win8, and optional for the same reason `NtQueryInformationByName`
    // below is: a host may not export it. Measured against GE-Proton11-6
    // (Wine 11.0 Staging), whose ntdll exports 18 of the 20 functions installed
    // here and omits exactly these two.
    //
    // Skipping it costs no coverage on such a host. `make_detour` fails because
    // `GetProcAddress` found nothing, and a symbol absent from ntdll's export
    // table is equally unreachable for the game — it cannot be resolved
    // dynamically and a static import against it would fail module load. So the
    // only reachable enumeration entry point there is `NtQueryDirectoryFile`,
    // hooked unconditionally just below.
    //
    // This is NOT licence to let enumeration go unhooked where the export does
    // exist: on Windows both are present and both are hooked, and a caller on an
    // unhooked enumeration path sees the real, near-empty folder and leaves no
    // trace anywhere. `skipped_detours()` reports what was passed over so a host
    // that expects total coverage can assert it rather than discover the hole
    // from a mod list that silently reads empty.
    // Tracked in a local rather than by reading `TRAMP_QDIREX` back: taking a
    // shared reference to a `static mut` is undefined behaviour if anything
    // mutates it concurrently, and `static_mut_refs` rightly denies it.
    let mut qdirex_installed = false;
    if let Ok(d_qdirex) = make_detour(ntdll, c"NtQueryDirectoryFileEx", qdirex_hook as *const ()) {
        TRAMP_QDIREX = Some(core::mem::transmute::<*const (), NtQueryDirectoryFileExFn>(
            d_qdirex.trampoline() as *const (),
        ));
        if d_qdirex.enable().is_ok() {
            detours.push(d_qdirex);
            qdirex_installed = true;
        } else {
            TRAMP_QDIREX = None;
        }
    }
    if !qdirex_installed {
        note_skipped_detour("NtQueryDirectoryFileEx");
    }
    // Both enumeration exports must be covered: whichever one the caller picks
    // decides whether it sees the composed tree or the real, near-empty folder
    // behind it, and a caller on the unhooked one leaves no trace anywhere.
    let d_qdir = make_detour(ntdll, c"NtQueryDirectoryFile", qdir_hook as *const ())?;
    TRAMP_QDIR = Some(core::mem::transmute::<*const (), NtQueryDirectoryFileFn>(
        d_qdir.trampoline() as *const (),
    ));
    // The path-based delete. It is not optional and not a nicety: it takes no
    // handle, so an unhooked call resolves the `OBJECT_ATTRIBUTES` path itself
    // and deletes the file that is really there, under a managed root or not.
    // See `delete_hook`.
    let d_delete = make_detour(ntdll, c"NtDeleteFile", delete_hook as *const ())?;
    TRAMP_DELETE = Some(core::mem::transmute::<*const (), NtDeleteFileFn>(
        d_delete.trampoline() as *const (),
    ));
    let d_close = make_detour(ntdll, c"NtClose", close_hook as *const ())?;
    TRAMP_CLOSE = Some(core::mem::transmute::<*const (), NtCloseFn>(
        d_close.trampoline() as *const (),
    ));
    let d_qif = make_detour(ntdll, c"NtQueryInformationFile", qif_hook as *const ())?;
    TRAMP_QIF = Some(core::mem::transmute::<*const (), NtQueryInformationFileFn>(
        d_qif.trampoline() as *const (),
    ));
    let d_setinfo = make_detour(ntdll, c"NtSetInformationFile", setinfo_hook as *const ())?;
    TRAMP_SETINFO = Some(core::mem::transmute::<*const (), NtSetInformationFileFn>(
        d_setinfo.trampoline() as *const (),
    ));
    let d_read = make_detour(ntdll, c"NtReadFile", read_hook as *const ())?;
    TRAMP_READ = Some(core::mem::transmute::<*const (), NtReadFileFn>(
        d_read.trampoline() as *const (),
    ));
    let d_write = make_detour(ntdll, c"NtWriteFile", write_hook as *const ())?;
    TRAMP_WRITE = Some(core::mem::transmute::<*const (), NtWriteFileFn>(
        d_write.trampoline() as *const (),
    ));
    let d_csec = make_detour(ntdll, c"NtCreateSection", create_section_hook as *const ())?;
    TRAMP_CREATE_SECTION = Some(core::mem::transmute::<*const (), NtCreateSectionFn>(
        d_csec.trampoline() as *const (),
    ));
    let d_map = make_detour(ntdll, c"NtMapViewOfSection", map_view_hook as *const ())?;
    TRAMP_MAP_VIEW = Some(core::mem::transmute::<*const (), NtMapViewOfSectionFn>(
        d_map.trampoline() as *const (),
    ));
    let d_unmap = make_detour(ntdll, c"NtUnmapViewOfSection", unmap_view_hook as *const ())?;
    TRAMP_UNMAP_VIEW = Some(core::mem::transmute::<*const (), NtUnmapViewOfSectionFn>(
        d_unmap.trampoline() as *const (),
    ));
    let d_qvol =
        make_detour(ntdll, c"NtQueryVolumeInformationFile", qvol_hook as *const ())?;
    TRAMP_QVOL = Some(core::mem::transmute::<*const (), NtQueryVolumeInformationFileFn>(
        d_qvol.trampoline() as *const (),
    ));
    // The lock/flush trio. Without these a synthetic handle is not merely
    // missing a feature — the *next* call after a successful open fails, and
    // the caller abandons the file entirely. See `lock_hook`.
    let d_lock = make_detour(ntdll, c"NtLockFile", lock_hook as *const ())?;
    TRAMP_LOCK = Some(core::mem::transmute::<*const (), NtLockFileFn>(
        d_lock.trampoline() as *const (),
    ));
    let d_unlock = make_detour(ntdll, c"NtUnlockFile", unlock_hook as *const ())?;
    TRAMP_UNLOCK = Some(core::mem::transmute::<*const (), NtUnlockFileFn>(
        d_unlock.trampoline() as *const (),
    ));
    let d_flush = make_detour(ntdll, c"NtFlushBuffersFile", flush_hook as *const ())?;
    TRAMP_FLUSH = Some(core::mem::transmute::<*const (), NtFlushBuffersFileFn>(
        d_flush.trampoline() as *const (),
    ));

    // Present since Win10 1709. Optional so an older host still installs.
    let mut qibn_installed = false;
    if let Ok(d_qibn) = make_detour(ntdll, c"NtQueryInformationByName", qibn_hook as *const ()) {
        TRAMP_QIBN = Some(core::mem::transmute::<*const (), NtQueryInformationByNameFn>(
            d_qibn.trampoline() as *const (),
        ));
        if d_qibn.enable().is_ok() {
            detours.push(d_qibn);
            qibn_installed = true;
        } else {
            TRAMP_QIBN = None;
        }
    }
    if !qibn_installed {
        note_skipped_detour("NtQueryInformationByName");
    }

    // The other half of handle identity, beside `NtQueryInformationFile`'s
    // class-48 spoof: `GetFinalPathNameByHandleW` reaches
    // `NtQueryInformationFile` on Windows and `NtQueryObject` here on Wine, so
    // leaving this one unhooked leaks the backing path on whichever host takes
    // the other route. Optional in the same style as the two above only because
    // a host might not export it — both hosts measured here do, so
    // `skipped_detours()` stays empty and `tests/hook_coverage.rs` asserts it.
    let mut qobj_installed = false;
    if let Ok(d_qobj) = make_detour(ntdll, c"NtQueryObject", qobj_hook as *const ()) {
        TRAMP_QOBJ = Some(core::mem::transmute::<*const (), NtQueryObjectFn>(
            d_qobj.trampoline() as *const (),
        ));
        if d_qobj.enable().is_ok() {
            detours.push(d_qobj);
            qobj_installed = true;
        } else {
            TRAMP_QOBJ = None;
        }
    }
    if !qobj_installed {
        note_skipped_detour("NtQueryObject");
    }

    d_qdir.enable().map_err(|_| InstallError::Detour)?;
    d_delete.enable().map_err(|_| InstallError::Detour)?;
    d_close.enable().map_err(|_| InstallError::Detour)?;
    d_qif.enable().map_err(|_| InstallError::Detour)?;
    d_setinfo.enable().map_err(|_| InstallError::Detour)?;
    d_read.enable().map_err(|_| InstallError::Detour)?;
    d_write.enable().map_err(|_| InstallError::Detour)?;
    d_csec.enable().map_err(|_| InstallError::Detour)?;
    d_map.enable().map_err(|_| InstallError::Detour)?;
    d_unmap.enable().map_err(|_| InstallError::Detour)?;
    d_qvol.enable().map_err(|_| InstallError::Detour)?;
    d_lock.enable().map_err(|_| InstallError::Detour)?;
    d_unlock.enable().map_err(|_| InstallError::Detour)?;
    d_flush.enable().map_err(|_| InstallError::Detour)?;
    // Every enabled detour must be kept alive here: dropping one silently
    // un-patches it, which reads exactly like "the process never calls this".
    detours.extend([
        d_qdir, d_delete, d_close, d_qif, d_setinfo, d_read, d_write, d_csec, d_map,
        d_unmap, d_qvol, d_lock, d_unlock, d_flush,
    ]);

    // Best-effort child-process propagation + virtual image path spoof.
    if let Some(dll) = self_dll_path() {
        let _ = SELF_DLL.set(dll);
        let mut kb = GetModuleHandleA(c"kernelbase.dll".as_ptr().cast());
        if kb.is_null() {
            kb = GetModuleHandleA(c"kernel32.dll".as_ptr().cast());
        }
        if !kb.is_null() {
            if let Ok(d_cpiw) = make_detour(kb, c"CreateProcessInternalW", cpiw_hook as *const ())
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

/// The directory that a relative name is expressed against, plus whether
/// finding it required consulting the OS about a handle's *current* target
/// rather than reading something the shim already knew deterministically.
/// See [`DecodedPath`] for why that distinction has to travel with the path.
///
/// Four kinds of parent reach us, and missing any one makes the child
/// undecodable — which is silent rather than an error: the call simply bypasses
/// every decision we would have made and lands on whatever is really on disk.
/// Shared by every hook that has to decode a name, so they cannot drift apart.
unsafe fn parent_dir_of_handle(root_handle: HANDLE) -> Option<(String, bool)> {
    let root = root_handle as isize;
    // 1. Our own synthetic directory handles.
    if crate::fuse_synth::is_fuse_synth(root) {
        // Prefer PATH_TABLE (recorded on open); fall back to fuse_synth abs_path.
        let p = PATH_TABLE
            .lock()
            .ok()
            .and_then(|t| t.get(&root).cloned())
            .or_else(|| crate::fuse_synth::abs_path(root))?;
        return Some((p, false));
    }
    // 2. A real directory the process opened; we remember every one.
    if let Some(p) = path_of_handle(root_handle) {
        return Some((p, false));
    }
    // 3. The current-directory handle. The OS creates it, so it is in no table
    //    of ours, yet it is the parent for every relative open a CRT makes:
    //    `CreateFileW("Data\X")` becomes (CWD handle + "Data\X").
    if let Some((cwd_handle, dos)) = cwd_from_peb() {
        if cwd_handle == root {
            return Some((format!(r"\??\{}", dos.trim_end_matches(['\\', '/'])), false));
        }
    }
    // 4. A handle we never saw opened — opened before injection, inherited
    //    across a `CreateProcess`, or duplicated in from another process —
    //    so it appears in none of our tables and is not the PEB's CWD
    //    handle either. This is `NtCreateFile`'s
    //    `OBJECT_ATTRIBUTES.RootDirectory` vector: the game holds a real
    //    directory handle and names the child only relative to it, so the
    //    string a hook sees (`Skyrim\Data\a.esp`) cannot be related to the
    //    managed root by any amount of string canonicalisation — the root
    //    information lives in the handle, not the string. Ask the OS
    //    directly: `GetFinalPathNameByHandleW` on the handle itself needs no
    //    reopen, since we already hold it.
    //
    //    Its answer is `VOLUME_NAME_DOS` (`\\?\`-prefixed), not the `\??\`
    //    spelling a real NT open presents, but that is not parsed here —
    //    `path_of`'s callers always re-canonicalise the assembled path
    //    (`decision_for` -> `RootMap::under_root`, `path_is_ours` ->
    //    `RootMap::contains`), and `canonicalise` already treats `\\?\` as a
    //    recognised prefix. Handing back the OS string unparsed is exactly
    //    what `vfs_redirect::expand_short_name`'s callers already do with
    //    this same result shape (see its doc comment) — hand-parsing it here
    //    instead would be a second, drifting implementation of that same
    //    normalisation.
    //
    //    Expected to fire rarely: every handle the shim itself sees opened
    //    (case 2, above) is already free to answer from that table, whether
    //    or not it lies under the root — this branch is reached only for a
    //    handle the shim was not present to observe. Its cost is coupled to
    //    `tag_under_root` recording *every* handle unconditionally (not just
    //    ones under the root) — see that function's own doc comment.
    //
    //    SAFETY: `root_handle` is `OBJECT_ATTRIBUTES.RootDirectory` from an
    //    in-flight NT call this process is making right now — by
    //    construction a currently-valid, open handle owned by the caller
    //    (the game), which is exactly what `final_path_for_handle` requires.
    //    This function does not close it or otherwise take ownership of it.
    let resolved = vfs_win::final_path_for_handle(root_handle)?;
    // `true`: this string is a snapshot of the handle's target *right now*,
    // not a pure function of anything in `root_handle`/the relative name
    // bytes — every caller that turns this into a `RootMap`-backed decision
    // must say so too (`vfs_redirect::UncachedScope`), or the decision could
    // be cached under a string that stops being true later in the session.
    Some((resolved, true))
}

unsafe fn oa_name_only(oa: *const ObjectAttributes) -> Option<String> {
    if oa.is_null() {
        return None;
    }
    object_name_str(oa)
}

/// A path decoded from an `OBJECT_ATTRIBUTES`, tagged with whether decoding it
/// required consulting the OS about a handle's current target
/// (`parent_dir_of_handle`'s case 4) rather than being derivable purely from
/// the shim's own tables, the raw name string, or the process's own PEB.
///
/// `os_consulted` is the caller-side half of `vfs_redirect::UncachedScope`'s
/// contract: any `RootMap`-backed decision made with `path` — directly via
/// `decision_for`, or indirectly via `path_is_ours` — must hold that guard
/// for as long as it is deciding with `path`, because the answer is itself
/// only a fact about a handle's target *at this moment*, not a pure function
/// of `path`'s bytes that would be safe to cache under them.
struct DecodedPath {
    path: String,
    os_consulted: bool,
}

/// Decode `oa` to a full path, once, tracking whether the decode needed an OS
/// consult. [`path_of`] is the provenance-blind convenience wrapper for the
/// many callers (filename matching, tracing, hookstats) that only ever read
/// the string and never feed it back into a cached `RootMap` decision.
unsafe fn path_of_tracked(oa: *const ObjectAttributes) -> Option<DecodedPath> {
    if oa.is_null() {
        return None;
    }
    let oa_ref = &*oa;
    let name = object_name_str(oa)?;
    if oa_ref.root_directory.is_null() {
        return if name.is_empty() {
            None
        } else {
            Some(DecodedPath { path: name, os_consulted: false })
        };
    }
    let (parent, os_consulted) = parent_dir_of_handle(oa_ref.root_directory)?;
    let parent = parent.trim_end_matches(['\\', '/']);
    let rel = name.trim_start_matches(['\\', '/']);
    let path = if rel.is_empty() { parent.to_string() } else { format!("{parent}\\{rel}") };
    Some(DecodedPath { path, os_consulted })
}

unsafe fn path_of(oa: *const ObjectAttributes) -> Option<String> {
    path_of_tracked(oa).map(|d| d.path)
}

/// Decide what to do with an already-decoded `path`, given its access mask
/// and create disposition (write-path aware). Takes the path rather than
/// `oa` so a single invocation's decode (`path_of_tracked`, done once by
/// `create_hook`/`open_hook`) is reused rather than re-run here — see
/// `tag_under_root`'s doc comment for why re-running it matters for cost, not
/// just style.
///
/// Caller's responsibility, not this function's: if the path came from an
/// OS-consulted decode, hold a `vfs_redirect::UncachedScope` around this call
/// (and any other `RootMap`-backed call made with the same path) so the
/// answer is never cached under it.
fn decision_for(path: Option<&str>, access: u32, disposition: u32) -> Option<Decision> {
    let engine = ENGINE.get()?;
    let path = path?;
    Some(engine.decide_open(path, access, disposition))
}

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

/// Record a freshly-opened handle as a candidate directory for enumeration
/// virtualization: only when the open succeeded and its path is under the
/// managed root. Harmless for file handles (they never receive a dir-enum call)
/// and reclaimed by `NtClose`. Shared by the `NtCreateFile` and `NtOpenFile`
/// pass-through paths.
///
/// Takes the already-decoded `path` rather than `oa`: `create_hook`/
/// `open_hook` decode once per invocation (`path_of_tracked`) and thread the
/// result through every function that used to call `path_of(oa)`
/// independently — including this one, `record_path`, `record_identity`,
/// `note_decision_outcome`, and `note_passthrough_outcome`. Before that, a
/// single hooked `NtCreateFile` could re-run the decode 2-5 times over; for
/// an unresolved handle-relative open that decode is `parent_dir_of_handle`'s
/// OS-consulted case 4 (a `GetFinalPathNameByHandleW` call), so re-running it
/// per caller meant several syscalls per open rather than one. Resolving once
/// and passing the `&str` down is safe to do — nothing changes underneath a
/// single hook invocation's decoded path in the window between its callers —
/// which is a different claim from *caching* it across invocations, and must
/// not be confused with the caching `vfs_redirect::UncachedScope` forbids for
/// an OS-consulted path (see `parent_dir_of_handle`'s case 4).
///
/// Caller's responsibility: hold a `vfs_redirect::UncachedScope` around this
/// call if `path` came from an OS-consulted decode — `path_is_ours` below
/// reaches the same cached `RootMap::under_root` `decision_for` does.
unsafe fn tag_under_root(file_handle: *mut HANDLE, path: Option<&str>, status: NTSTATUS) {
    // NT_SUCCESS is status >= 0.
    if status < 0 || file_handle.is_null() {
        return;
    }
    let Some(path) = path else { return };
    let key = *file_handle as isize;
    // Remember every handle's path, not just the ones under the root. NT lets a
    // caller open a file as (directory handle + leaf name), and without the
    // parent's path such an open cannot be decoded at all -- it is invisible to
    // every decision we make and reaches the real directory behind the mount.
    // The parent is often outside the root while the child is under it.
    //
    // Load-bearing for cost, not just correctness: `parent_dir_of_handle`'s
    // case 4 (OS-consulted fallback for a handle unseen by the shim) reasons
    // that it fires rarely *because* every handle the shim does see reach a
    // hooked create/open lands here unconditionally. Narrowing this insert to
    // only under-root handles (matching the `DIR_TABLE` insert just below)
    // would make case 4 fire for every outside-root ancestor handle too,
    // changing that branch from a rare safety net into a per-open cost.
    if let Ok(mut t) = HANDLE_PATHS.lock() {
        crate::breadcrumb::set_holder(crate::breadcrumb::holder::TAG_UNDER_ROOT);
        if t.len() < HANDLE_PATHS_MAX {
            t.insert(key, path.to_string());
        }
        crate::breadcrumb::set_holder(crate::breadcrumb::holder::NOBODY);
    }
    if path_is_ours(path) {
        if let Ok(mut table) = DIR_TABLE.lock() {
            table.insert(key, DirTracked { dir_nt_path: path.to_string(), state: None });
        }
    }
}

/// Handle -> the NT path it was opened as, for *every* successful open.
///
/// Reclaimed by `NtClose`; bounded so a handle leak cannot grow it without end.
static HANDLE_PATHS: Mutex<BTreeMap<isize, String>> = Mutex::new(BTreeMap::new());
const HANDLE_PATHS_MAX: usize = 65_536;

fn path_of_handle(handle: HANDLE) -> Option<String> {
    let g = HANDLE_PATHS.lock().ok()?;
    crate::breadcrumb::set_holder(crate::breadcrumb::holder::PATH_OF_HANDLE);
    let r = g.get(&(handle as isize)).cloned();
    crate::breadcrumb::set_holder(crate::breadcrumb::holder::NOBODY);
    r
}

/// Record a redirected handle's virtual identity: after a successful redirected
/// open, map the handle to the volume-relative form of the ORIGINAL virtual
/// path (the caller's `oa` still held it — only a local `new_oa` was
/// rewritten before the trampoline call). Reclaimed by `NtClose`. Enables the
/// `NtQueryInformationFile` class-48 spoof.
///
/// Takes the already-decoded `path` — see `tag_under_root`'s doc comment for
/// why callers thread this through rather than re-decoding independently.
unsafe fn record_identity(file_handle: *mut HANDLE, path: Option<&str>, status: NTSTATUS) {
    if status < 0 || file_handle.is_null() {
        return;
    }
    if let Some(path) = path {
        if let Ok(mut t) = IDENTITY_TABLE.lock() {
            t.insert(*file_handle as isize, nt_to_volume_relative(path));
        }
    }
}

/// Record a successful under-root open's handle -> folded vpath components, so
/// a later handle-based delete/rename can act by vpath. Shared by both open
/// hooks across all decision branches.
///
/// Takes the already-decoded `path` — see `tag_under_root`'s doc comment for
/// why callers thread this through rather than re-decoding independently.
/// Caller's responsibility: hold a `vfs_redirect::UncachedScope` around this
/// call if `path` came from an OS-consulted decode (`path_is_ours` below
/// reaches the same cached `RootMap::under_root` `decision_for` does).
unsafe fn record_path(file_handle: *mut HANDLE, path: Option<&str>, status: NTSTATUS) {
    if status < 0 || file_handle.is_null() {
        return;
    }
    if let Some(path) = path {
        if path_is_ours(path) {
            if let Ok(mut t) = PATH_TABLE.lock() {
                t.insert(*file_handle as isize, path.to_string());
            }
        }
    }
}

/// Is this path one we are responsible for?
///
/// There are two notions of "ours" and they are still not quite the same.
/// Both are `RootMap`s now, both canonicalise identically, and since gate 4
/// Task 3 both are told about every root the session declared — `bootstrap.rs`
/// builds the engine's list with the client's own `roots_from_env`. What is
/// left is one deliberate difference: the client also declares the staging
/// directory as an alias for root 0 (a staged game's working directory is
/// that staging directory, so it reaches our content by that name), and the
/// engine deliberately does not. **The reason for that asymmetry was the DRM
/// exceptions, which gate 5 Task 4 deleted** — they trampolined to real files
/// in exactly that directory and needed the engine to call it outside. The
/// omission is now inert rather than load-bearing: staged-directory opens are
/// answered by the client before the engine is consulted at all. It is kept
/// because the client's root set being a superset of the engine's is what makes
/// "under an engine root but outside every client root" impossible, which
/// several arms rely on — see `bootstrap.rs` for the full argument.
///
/// So the narrow question still disowns the aliased half, and every caller
/// must keep asking through here.
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
    //
    // NOTE: discards `parent_dir_of_handle`'s OS-consulted provenance bit.
    // Its case-4 fallback can fire here exactly as it can for a handle-relative
    // create/open, and this rename/delete path's callers (`engine.rename`/
    // `engine.whiteout`) are `RootMap`-backed and cached the same way
    // `decision_for` is — so an OS-consulted rename target has the same
    // caching exposure `create_hook`/`open_hook` were fixed for, not yet
    // closed here. Tracked as a known gap rather than silently assumed safe.
    let (parent, _os_consulted) = parent_dir_of_handle(root_dir as HANDLE)?;
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
    const WRITE_ACCESS: u32 = 0x4000_0000 | 0x0002 | 0x0004; // GENERIC_WRITE|FILE_WRITE_DATA|FILE_APPEND_DATA
    (access & WRITE_ACCESS) != 0 || matches!(disposition, 0 | 2 | 3 | 4 | 5)
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
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_WRITE_DATA: u32 = 0x0002;
    const FILE_APPEND_DATA: u32 = 0x0004;
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
    let client = crate::fuse_client::global()?;
    let path = path?.to_string();
    let (root, vpath) = client.vpath_under_root(&path)?;
    // Directory open of root: empty vpath → "."
    let vp = if vpath.is_empty() { "." } else { vpath.as_str() };

    // **Gate 5, Task 4 — the DRM/identity exceptions, closed.** Four basenames
    // (`steam_appid.txt`, `SkyrimSELauncher.exe`, `steam_api{,64}.dll`,
    // `SkyrimSE.exe`) used to be matched here, case-insensitively at any depth,
    // and returned `None` *before the ring was consulted* — sending the open on
    // to `decision_for`, which either redirected it at a real disk path or
    // passed it straight through to the real filesystem under the managed root.
    // That was the last route by which a path under a managed root reached
    // something other than the director.
    //
    // The reason recorded for keeping them was "serving `SkyrimSE.exe` through
    // FUSE produced a Steam Error, caused by an open that fails to resolve
    // (FUSE-relative `OBJECT_ATTRIBUTES` reaching the kernel)". That reason was
    // self-cancelling: the unresolvable OA only ever arose on the *excepted*
    // arm, which is the one that has to hand the kernel an OA whose root is a
    // synthetic handle (see `tramp_create_abs`). With the exception gone the
    // kernel is never called for these names at all — the open either gets a
    // synthetic handle from the director or is sealed.
    //
    // Two things the deleted comment got right and are worth keeping: Steam
    // does **not** compare the in-memory image against the on-disk PE (measured
    // — the whole loaded image was once overwritten with zip PE bytes at a
    // relocated base and DRM still verified), and what actually needs the
    // on-disk exe is outside this hook: `CreateProcess` of the host image, and
    // Steam's own path association from a separate, un-injected process.
    //
    // `OpenOutcome::FellThroughDrmException` is deliberately kept in the enum
    // and in the report reading **zero**: a removed counter cannot prove the
    // class stayed closed, and the shim/director reconciliation asserts on it.
    //
    // The tracer stays wired for the live acceptance run — it is off unless
    // `VFS_DRM_EXE_LOG` names a file, and it now sees the opens it never could
    // before, since these names finally arrive here.
    drm_exe_trace(&path, fuse_root_directory(oa), write);

    // Every under-root open — read *and* write — goes through the
    // director (zip / composed / writable layer), and every answer it gives,
    // including the failures, is this function's answer too: since gate 4's
    // Task 5 no *decision* below returns `None`, except behind the explicit
    // `allow_disk_fallthrough` opt-out.
    //
    // One `None` below is not a decision: `open_fuse_at_ex(...)?` on the
    // success path gives up its handle if the synth table's mutex is poisoned,
    // which sends the caller to `decision_for` after the director has already
    // opened the file — and leaks that `fh`, since nothing closes it. It
    // pre-dates this task, and it is a real hole in "the director's answer is
    // the caller's answer", so do not read the paragraph above as more
    // absolute than it is.
    //
    // It is still not a live route — but not for the reason this comment used
    // to give. It claimed the crate builds with `panic = "abort"`; it does
    // not. `rust/Cargo.toml` sets `panic = "unwind"` for both profiles,
    // deliberately, so "a panic cannot unwind here" is simply false and
    // nothing about poisoning is ruled out by the profile. Two independent
    // reasons rule it out instead:
    //
    //  1. Nothing inside those critical sections can unwind. `fuse_synth`
    //     holds `TABLE`/`NEXT` across `usize` arithmetic, `BTreeMap`
    //     insert/get/get_mut/remove keyed by `usize`, and `String`
    //     clone/drop — no `unwrap`, no slice indexing, no caller-supplied
    //     closure, no `Ord` or `Drop` impl that can panic. Allocation failure
    //     aborts rather than unwinding. Poisoning requires a panic to unwind
    //     *out of a held guard*, and there is no panic here to unwind.
    //  2. Even granting one, every production path into this code arrives
    //     through an `unsafe extern "system"` hook, and rustc's forced
    //     abort-on-unwind at a non-`-unwind` `extern` boundary tears the
    //     process down while that unwind is still in flight. The guard's drop
    //     would set the poison flag on the way out, but no later call would
    //     be alive to observe it.
    //
    // Reason 1 is the one to re-check if `fuse_synth` ever grows a fallible
    // or reentrant operation under those locks; reason 2 holds only for the
    // injected process, not for in-process tests that drive these paths
    // directly.
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
        client.open_write(root, vp, create_flags)
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
            if !file_handle.is_null() {
                *file_handle = h as HANDLE;
            }
            if !iosb.is_null() {
                let p = iosb as *mut u8;
                core::ptr::write_unaligned(p as *mut u32, STATUS_SUCCESS as u32);
                let info = if write {
                    disposition_information(disposition, existed_before)
                } else {
                    crate::ntdef::FILE_OPENED
                };
                core::ptr::write_unaligned(p.add(8) as *mut usize, info);
            }
            // Direct PATH_TABLE insert with absolute path (path_of may be relative OA).
            if let Ok(mut t) = PATH_TABLE.lock() {
                t.insert(h, path.clone());
            }
            record_path(file_handle, Some(&path), STATUS_SUCCESS);
            if resp.is_dir {
                tag_under_root(file_handle, Some(&path), STATUS_SUCCESS);
            }
            director_open_trace(&path, resp.size);
            Some(STATUS_SUCCESS)
        }
        // Not in director: seal the path, for reads and writes alike. The
        // *only* way out of this arm without a status is the explicit
        // `VFS_ALLOW_DISK_FALLTHROUGH` opt-out, which unseals the root
        // wholesale (see `allow_disk_fallthrough`) and is off by default and
        // cleared defensively by `skyrim-live`.
        //
        // **Gate 4, Task 5 — this is the write fall-through, closed.** A write
        // used to return `None` here unconditionally, which sends
        // `create_hook`/`open_hook` on to `decision_for` -> `Engine::decide_open`:
        // an overlay redirect where one is configured, and a plain pass-through
        // to the real filesystem *under the managed root* where one is not.
        // Both spellings put content the provider graph never saw somewhere the
        // director cannot account for; the pass-through one physically creates a
        // file under a root whose whole contract is that the real filesystem
        // beneath it is unreachable.
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
                // Two different failures wear `ST_NOT_FOUND` on a write open,
                // and NT distinguishes them, so this must too:
                //
                // - The open asked to **create** (any of SUPERSEDE / CREATE /
                //   OPEN_IF / OVERWRITE_IF set `OPEN_CREATE`) and the director
                //   still said not-found: no writable provider is mounted
                //   anywhere over this path. The name is not what is missing —
                //   the caller was going to supply it — so this is
                //   `STATUS_OBJECT_PATH_NOT_FOUND` (`ERROR_PATH_NOT_FOUND`),
                //   the same answer NTFS gives for a create whose containing
                //   directory does not exist.
                // - The open did **not** ask to create (FILE_OPEN /
                //   FILE_OVERWRITE with write access — "open the existing
                //   file for writing"). Then the file itself is simply absent
                //   and the honest answer is the ordinary
                //   `STATUS_OBJECT_NAME_NOT_FOUND` (`ERROR_FILE_NOT_FOUND`) —
                //   the same one the read seal below returns. Answering
                //   PATH_NOT_FOUND here would mislead the very common
                //   "open-for-write, and on ERROR_FILE_NOT_FOUND create it"
                //   idiom into thinking the directory was gone.
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
        // Any other director error on a write. **Gate 4, Task 5:** this used to
        // return `None` — "the director rejects OPEN_WRITE, so let the write
        // land in the shim-local overlay instead" — which is the second half of
        // the fall-through this task closes.
        //
        // Deliberately *not* merged with the `ST_NOT_FOUND` arm above. That one
        // is a path no provider serves; this one is a provider that served the
        // path and then refused or failed the write, and the two want different
        // answers at the NT boundary:
        //
        // - `ST_READ_ONLY` is the director's own policy status, meaning "no
        //   `ReadWrite` provider serves this path" (`Director::open`, which
        //   also records it for `vfs stats` discovery). That is a permission
        //   fact, not a fault, and `STATUS_ACCESS_DENIED` is what a real
        //   read-only filesystem answers — a status callers already have code
        //   for, unlike `STATUS_UNSUCCESSFUL`'s `ERROR_GEN_FAILURE`.
        // - `ST_IS_DIR` means the path is a directory and the caller asked to
        //   create or replace a file over it (`OverlayProvider::open_for_write`
        //   refuses rather than letting a `DiskProvider` upper create a file
        //   named after the directory). The two non-creating dispositions
        //   never get here — `dir_open_downgrades` already turned them into
        //   the directory open the caller meant — so what is left genuinely
        //   is a file create aimed at a directory, and NT has a status that
        //   says exactly that.
        // - Anything else (I/O error, bad request, a provider that broke) is a
        //   genuine failure: `STATUS_UNSUCCESSFUL`, matching the read-side
        //   `Err(_)` arm below, which likewise refuses to fall through.
        //
        // Note there is no `allow_disk_fallthrough` escape here, again matching
        // the read side: that switch relaxes "the director does not have this",
        // never "the director failed".
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
    let Some(path) = LOG.get_or_init(|| vfs_env::path(vfs_env::DRM_EXE_LOG)).as_ref() else {
        return;
    };
    if !std::path::Path::new(nt_or_win_path)
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|b| b.eq_ignore_ascii_case("SkyrimSE.exe"))
    {
        return;
    }
    let p = crate::fuse_client::strip_nt_device(nt_or_win_path.trim()).replace('/', "\\");
    let line = format!(
        "{}\tskyrimse-exe\toa={}\taccess={}\t{}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        if rel { "fuse-relative" } else { "absolute" },
        if write { "write" } else { "read" },
        p
    );
    let Some(_io) = ShimIoGuard::enter() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(line.as_bytes());
    }
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
    let Some(_io) = ShimIoGuard::enter() else {
        return;
    };
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
    let client = crate::fuse_client::global()?;
    let path = path?;
    let (root, vpath) = client.vpath_under_root(path)?;
    let vp = if vpath.is_empty() { "." } else { vpath.as_str() };
    match client.mkdir(root, vp, 0o755) {
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
            record_path(file_handle, Some(path), STATUS_SUCCESS);
            tag_under_root(file_handle, Some(path), STATUS_SUCCESS);
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
        Err(_) => match client.getattr(root, vp) {
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
                    record_path(file_handle, Some(path), STATUS_SUCCESS);
                    tag_under_root(file_handle, Some(path), STATUS_SUCCESS);
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
unsafe fn create_hook_body(
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
    // Decode once for the whole call and thread the result through every
    // function below that used to call `path_of(oa)` independently — see
    // `tag_under_root`'s doc comment for the cost argument.
    let decoded = path_of_tracked(oa);
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
    if let Some(st) = try_fuse_mkdir(file_handle, path, iosb, opts, disp) {
        return st;
    }
    // Prefer director FUSE for managed-root content (no in-shim zipserve).
    match path {
        Some(p) => crate::hookstats::note_passthrough(p),
        // An open we cannot decode is an open we cannot serve. If the masters
        // are hiding anywhere, it is here.
        None => crate::hookstats::note_undecodable(oa_name_only(oa).as_deref()),
    }
    // Set by `try_fuse_create` when it already recorded an outcome (the write
    // fallback — the DRM exception was the other one and is gone) for this
    // open before returning `None` — see `note_decision_outcome` below for why
    // that must suppress the `decision_for`-based recording that always runs
    // next.
    let mut outcome_recorded = false;
    if let Some(st) = try_fuse_create(
        file_handle,
        oa,
        path,
        iosb,
        is_write_open(access, disp),
        disp,
        open_create_flags(disp),
        is_append_only(access),
        &mut outcome_recorded,
    ) {
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
            record_identity(file_handle, path, status);
            record_path(file_handle, path, status);
            status
        }
        Some(Decision::Deny) => {
            note_decision_outcome(path, outcome_recorded, crate::hookstats::OpenOutcome::Denied);
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
            if fuse_root_directory(oa) {
                if let Some(path) = path {
                    let status = tramp_create_abs(
                        tramp, file_handle, access, oa, iosb, alloc, attrs, share, disp, opts,
                        ea, ealen, path,
                    );
                    tag_under_root(file_handle, Some(path), status);
                    record_path(file_handle, Some(path), status);
                    return status;
                }
                return STATUS_OBJECT_NAME_NOT_FOUND;
            }
            let status =
                tramp(file_handle, access, oa, iosb, alloc, attrs, share, disp, opts, ea, ealen);
            tag_under_root(file_handle, path, status);
            record_path(file_handle, path, status);
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
unsafe fn open_hook_body(
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
    // Decode once for the whole call — see `create_hook` and `tag_under_root`'s
    // doc comment for why, and for what the `UncachedScope` guard is for.
    let decoded = path_of_tracked(oa);
    let path: Option<&str> = decoded.as_ref().map(|d| d.path.as_str());
    let os_consulted = decoded.as_ref().is_some_and(|d| d.os_consulted);
    let _uncached_guard = os_consulted.then(vfs_redirect::UncachedScope::enter);

    match path {
        Some(p) => crate::hookstats::note_passthrough(p),
        None => crate::hookstats::note_undecodable(oa_name_only(oa).as_deref()),
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
    if let Some(st) = try_fuse_create(
        file_handle,
        oa,
        path,
        iosb,
        is_write_open(access, vfs_redirect::FILE_OPEN),
        vfs_redirect::FILE_OPEN,
        0,
        is_append_only(access),
        &mut outcome_recorded,
    ) {
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
            record_identity(file_handle, path, status);
            record_path(file_handle, path, status);
            status
        }
        Some(Decision::Deny) => {
            note_decision_outcome(path, outcome_recorded, crate::hookstats::OpenOutcome::Denied);
            STATUS_OBJECT_NAME_NOT_FOUND
        }
        Some(Decision::PassThrough) | None => {
            if is_passthrough {
                note_passthrough_outcome(path, outcome_recorded);
            }
            if fuse_root_directory(oa) {
                if let Some(path) = path {
                    let status =
                        tramp_open_abs(tramp, file_handle, access, oa, iosb, share, opts, path);
                    tag_under_root(file_handle, Some(path), status);
                    record_path(file_handle, Some(path), status);
                    return status;
                }
                return STATUS_OBJECT_NAME_NOT_FOUND;
            }
            let status = tramp(file_handle, access, oa, iosb, share, opts);
            tag_under_root(file_handle, path, status);
            record_path(file_handle, path, status);
            status
        }
    }
}

/// Path-based getattr via director OP_GETATTR when FUSE client is live.
/// `Some(...)` means the path is under the managed root — caller must not tramp
/// to the Steam tree on NOT_FOUND (seal under-root).
unsafe fn fuse_path_attr(path: &str) -> Option<Result<(bool, u64, i64), i32>> {
    let client = crate::fuse_client::global()?;
    let (root, vpath) = client.vpath_under_root(path)?;
    let vp = if vpath.is_empty() { "." } else { vpath.as_str() };
    Some(match client.getattr(root, vp) {
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
            core::ptr::write_unaligned(p.add(21), u8::from(is_dir)); // Directory
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

unsafe fn qibn_hook_body(
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
        // Task 4: the local snapshot no longer answers attribute queries (that
        // was `RootMap::query_attributes`/`AttrDecision`, both deleted) — the
        // director already had first refusal via `fuse_path_attr` above. The
        // shim-local write overlay (gate 4's mechanism) is the only thing
        // left that can still answer without the director, since it holds
        // content the director never sees (a just-created/modified file, or
        // a runtime delete's whiteout).
        if let Some(engine) = ENGINE.get() {
            match engine.overlay_state(&path) {
                Some(OverlayState::Present { is_dir, size, .. }) => {
                    if let Some(n) = fill_by_name(class_raw, info, length, is_dir, size) {
                        if !iosb.is_null() {
                            let q = iosb as *mut u8;
                            core::ptr::write_unaligned(q as *mut u32, STATUS_SUCCESS as u32);
                            core::ptr::write_unaligned(q.add(8) as *mut usize, n);
                        }
                        return STATUS_SUCCESS;
                    }
                }
                Some(OverlayState::Whiteout) => return STATUS_OBJECT_NAME_NOT_FOUND,
                Some(OverlayState::Absent) | None => {}
            }
        }
    }
    tramp(oa, iosb, info, length, class_raw)
}

unsafe fn qattr_hook_body(
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
                        (*info).creation_time = SYNTH_FILETIME;
                        (*info).last_access_time = SYNTH_FILETIME;
                        (*info).last_write_time = SYNTH_FILETIME;
                        (*info).change_time = SYNTH_FILETIME;
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
        // Task 4: overlay-only fallback (see `qibn_hook`'s comment on the
        // equivalent branch) — no more local snapshot answering here.
        if let Some(engine) = ENGINE.get() {
            match engine.overlay_state(&path) {
                Some(OverlayState::Present { is_dir, .. }) => {
                    if !info.is_null() {
                        (*info).creation_time = SYNTH_FILETIME;
                        (*info).last_access_time = SYNTH_FILETIME;
                        (*info).last_write_time = SYNTH_FILETIME;
                        (*info).change_time = SYNTH_FILETIME;
                        (*info).file_attributes =
                            if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
                    }
                    return STATUS_SUCCESS;
                }
                Some(OverlayState::Whiteout) => return STATUS_OBJECT_NAME_NOT_FOUND,
                Some(OverlayState::Absent) | None => {}
            }
        }
    }
    tramp(oa, info)
}

unsafe fn qfull_hook_body(
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
                        (*info).creation_time = SYNTH_FILETIME;
                        (*info).last_access_time = SYNTH_FILETIME;
                        (*info).last_write_time = SYNTH_FILETIME;
                        (*info).change_time = SYNTH_FILETIME;
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
        // Task 4: overlay-only fallback (see `qibn_hook`'s comment on the
        // equivalent branch) — no more local snapshot answering here.
        if let Some(engine) = ENGINE.get() {
            match engine.overlay_state(&path) {
                Some(OverlayState::Present { is_dir, size, .. }) => {
                    if !info.is_null() {
                        (*info).creation_time = SYNTH_FILETIME;
                        (*info).last_access_time = SYNTH_FILETIME;
                        (*info).last_write_time = SYNTH_FILETIME;
                        (*info).change_time = SYNTH_FILETIME;
                        (*info).allocation_size = size as i64;
                        (*info).end_of_file = size as i64;
                        (*info).file_attributes =
                            if is_dir { FILE_ATTRIBUTE_DIRECTORY } else { FILE_ATTRIBUTE_NORMAL };
                    }
                    return STATUS_SUCCESS;
                }
                Some(OverlayState::Whiteout) => return STATUS_OBJECT_NAME_NOT_FOUND,
                Some(OverlayState::Absent) | None => {}
            }
        }
    }
    tramp(oa, info)
}

/// Reclaim any tracking for a closing handle before the OS (possibly) reuses
/// its value.
unsafe fn close_hook_body(handle: HANDLE) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Close);
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::ENTER);
    let tramp = match TRAMP_CLOSE {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_TABLE);
        if let Some(fh) = crate::fuse_synth::close_fuse(handle as isize) {
            crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_CLIENT);
            if let Some(c) = crate::fuse_client::global() {
                crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_RING);
                let _ = c.close(fh);
                crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_DONE);
            }
        }
        crate::breadcrumb::mark(crate::breadcrumb::mark_close::FUSE_EXIT);
        return STATUS_SUCCESS;
    }
    if crate::zipserve::is_synth_section(handle as isize) {
        crate::breadcrumb::mark(crate::breadcrumb::mark_close::ZIP_TABLE);
        // Releasing shim-owned VA waits for the last view (NT semantics).
        if let Some(window) = crate::zipserve::close_section(handle as isize) {
            crate::breadcrumb::mark(crate::breadcrumb::mark_close::ZIP_REGION);
            crate::lazy_section::on_section_closed(window);
            crate::breadcrumb::mark(crate::breadcrumb::mark_close::ZIP_DONE);
        }
        crate::breadcrumb::mark(crate::breadcrumb::mark_close::ZIP_EXIT);
        return STATUS_SUCCESS;
    }
    // **`try_lock`, never `lock`.** This is best-effort reclamation, and a
    // blocking acquisition here hangs the process permanently.
    //
    // Traced 2026-09-02 with `VFS_SHIM_BREADCRUMB`, three reproductions:
    // `threads=1`, `current=NtClose`, `mark=TABLE_HANDLE_PATHS`,
    // `holder=CLOSE_HOOK`, `entries - exits = 2`, zero CPU, and immune to
    // `TerminateProcess`. The holder is `close_hook_body` — but the only
    // statement it holds the guard across is a `BTreeMap<isize, String>`
    // remove, which cannot close a handle and so cannot be a live outer frame
    // re-entering. The holder is a **dead** frame.
    //
    // A thread terminated while holding a `std::sync::Mutex` leaves it locked
    // **forever, and not poisoned** — so every `if let Ok(..)` in this crate is
    // no defence against it. At process exit Windows terminates every thread
    // but one before `DLL_PROCESS_DETACH`, and the surviving thread then closes
    // handles on its way out. `vfs_shim_dll`'s own `DllMain` records this exact
    // hazard for a different lock: "one killed mid-write leaves a lock the flush
    // waits on forever".
    //
    // `try_lock` cannot deadlock. Losing a reclamation is harmless: the entry
    // is keyed by a handle value that is about to become invalid, `HANDLE_PATHS`
    // is bounded by `HANDLE_PATHS_MAX` against unbounded growth, and the
    // process is on its way out in the case that matters.
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TABLES);
    if let Ok(mut table) = DIR_TABLE.try_lock() {
        table.remove(&(handle as isize));
    }
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TABLE_HANDLE_PATHS);
    if let Ok(mut t) = HANDLE_PATHS.try_lock() {
        crate::breadcrumb::set_holder(crate::breadcrumb::holder::CLOSE_HOOK);
        t.remove(&(handle as isize));
        crate::breadcrumb::set_holder(crate::breadcrumb::holder::NOBODY);
    }
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TABLE_IDENTITY);
    if let Ok(mut t) = IDENTITY_TABLE.try_lock() {
        t.remove(&(handle as isize));
    }
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TABLE_PATH);
    if let Ok(mut t) = PATH_TABLE.try_lock() {
        t.remove(&(handle as isize));
    }
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TRAMP);
    let r = tramp(handle);
    crate::breadcrumb::mark(crate::breadcrumb::mark_close::TRAMP_DONE);
    r
}

/// `NtDeleteFile` hook — the **path-based** delete (gate 5, Task 5).
///
/// **This was the one unhooked NT API that reached real disk.** The others on
/// this project's list all take a handle, so leaving one unhooked fails safely:
/// under a managed root the caller is holding a synthetic handle, the kernel
/// does not own it, and the call comes back `STATUS_INVALID_HANDLE`. This one
/// takes only an `OBJECT_ATTRIBUTES`. There is no handle to be wrong about, so
/// an unhooked call resolves the path itself and unlinks the real file sitting
/// under the root — which is the exact thing the root's contract says is
/// unreachable.
///
/// **The decision is made on the path, like `create_hook`/`open_hook`, and by
/// the same machinery.** `path_of_tracked` decodes the `OBJECT_ATTRIBUTES`
/// (including a handle-relative name, and including the FUSE-synthetic
/// `RootDirectory` a virtual directory handle produces), and the three
/// questions asked of that path below are the three the open hooks already ask,
/// in the same order:
///
/// 1. `FuseClient::vpath_under_root` — the director's own notion of the root.
///    If it claims the path, the director's answer is the caller's answer, both
///    ways: `OP_DELETE` accepted is `STATUS_SUCCESS`, `OP_DELETE` refused is a
///    failure the caller sees. It never continues to the kernel from here, for
///    the same reason `try_fuse_create` does not: a refusal that falls through
///    is not a refusal.
/// 2. `Engine::whiteout` — the shim-local overlay, which is what
///    `setinfo_hook`'s non-synthetic branch already does for a *handle*-based
///    delete of the same path. Live when the director is absent (a `FuseClient`
///    that failed to attach still leaves an `Engine` with every declared root
///    and its overlay), and having both delete routes answer through the same
///    call is the point — two predicates that disagree about one path is the
///    failure mode this project has paid for twice.
/// 3. `path_is_ours` — the backstop. `Engine::whiteout` answers `false` for a
///    path it resolves with an *empty* remainder (the root directory itself)
///    and for an engine with no overlay at all, and `false` there must not mean
///    "let the kernel have it". Under a managed root that is the escape, not a
///    fallback, so it fails closed with `STATUS_ACCESS_DENIED` — distinct from
///    the director's own `STATUS_UNSUCCESSFUL` refusal above, because these are
///    different answers: one is "the graph said no", the other is "nothing here
///    is willing to answer, and the real file is not on offer".
///
/// Outside every root the call is trampolined unchanged, which is most of them
/// — a hook with an opinion about every delete in the process would break the
/// rest of it.
unsafe fn delete_hook_body(oa: *const ObjectAttributes) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::DeleteFile);
    let tramp = match TRAMP_DELETE {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // Shim-initiated I/O (overlay writes, the panic log, copy-up) must reach
    // the real ntdll, exactly as in `create_hook`/`open_hook`.
    if in_hook_reenter() {
        return tramp(oa);
    }
    // Decode once, and hold the `UncachedScope` for as long as this call
    // decides with the result — `vpath_under_root`, `whiteout` and
    // `path_is_ours` are all `RootMap`-backed and cached the same way
    // `decision_for` is. See `parent_dir_of_handle`'s case 4 and
    // `DecodedPath`'s doc comment.
    let decoded = path_of_tracked(oa);
    let _uncached_guard =
        decoded.as_ref().is_some_and(|d| d.os_consulted).then(vfs_redirect::UncachedScope::enter);
    let Some(path) = decoded.as_ref().map(|d| d.path.as_str()) else {
        // An undecodable delete is an undecodable open by another name: it
        // bypasses every decision we would have made. Recorded rather than
        // silently trampolined, so it shows up in the same place.
        crate::hookstats::note_undecodable(oa_name_only(oa).as_deref());
        return tramp(oa);
    };

    if let Some(client) = crate::fuse_client::global() {
        if let Some((root, vpath)) = client.vpath_under_root(path) {
            let vp = if vpath.is_empty() { "." } else { vpath.as_str() };
            return match client.delete(root, vp) {
                Ok(()) => STATUS_SUCCESS,
                Err(st) => delete_status_for(st),
            };
        }
    }
    if let Some(engine) = ENGINE.get() {
        if engine.whiteout(path) {
            return STATUS_SUCCESS;
        }
    }
    if path_is_ours(path) {
        return STATUS_ACCESS_DENIED;
    }
    // Outside every root. A FUSE-synthetic `RootDirectory` is invalid to the
    // kernel even here, so rebuild the OA absolute rather than hand the
    // synthetic handle over — the same narrow disagreement case
    // `tramp_create_abs` documents.
    if fuse_root_directory(oa) {
        return tramp_delete_abs(tramp, oa, path);
    }
    tramp(oa)
}

/// The NT status for a director `OP_DELETE` refusal.
///
/// **Flattening every refusal to `STATUS_UNSUCCESSFUL` is not neutral.** That
/// maps to `ERROR_GEN_FAILURE`, and the delete-then-create idiom — the single
/// most common thing callers do with a delete — treats only
/// `ERROR_FILE_NOT_FOUND` as benign and gives up on anything else. So a delete
/// of a path the director simply does not have would stop callers that a real
/// filesystem lets straight through. The open path already distinguishes these
/// (`try_fuse_create`'s `Err` arms); this is the same mapping for the same
/// reason, kept as one function so the two cannot drift:
///
/// - `ST_NOT_FOUND` -> `STATUS_OBJECT_NAME_NOT_FOUND` (`ERROR_FILE_NOT_FOUND`).
///   Nothing to delete is not a failure to delete.
/// - `ST_READ_ONLY` -> `STATUS_ACCESS_DENIED`. The director's own policy status
///   for "no `ReadWrite` provider serves this path", and `ERROR_ACCESS_DENIED`
///   is what a real read-only filesystem answers a `DeleteFileW`.
/// - `ST_IS_DIR` -> `STATUS_FILE_IS_A_DIRECTORY`, which `RtlNtStatusToDosError`
///   folds to `ERROR_ACCESS_DENIED` — exactly what `DeleteFileW` returns when
///   the name is a directory.
/// - Anything else (I/O error, a provider that broke) is a genuine failure and
///   keeps `STATUS_UNSUCCESSFUL`.
fn delete_status_for(st: i32) -> NTSTATUS {
    match st {
        vfs_protocol::ST_NOT_FOUND => STATUS_OBJECT_NAME_NOT_FOUND,
        vfs_protocol::ST_READ_ONLY => STATUS_ACCESS_DENIED,
        vfs_protocol::ST_IS_DIR => STATUS_FILE_IS_A_DIRECTORY,
        _ => STATUS_UNSUCCESSFUL,
    }
}

/// `NtDeleteFile` via the trampoline with an absolute NT path and a **null**
/// `RootDirectory`. The `NtDeleteFile` counterpart of [`tramp_create_abs`];
/// see that function for when a synthetic root reaches a fall-through at all.
unsafe fn tramp_delete_abs(
    tramp: NtDeleteFileFn,
    oa: *const ObjectAttributes,
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
    let status = tramp(&new_oa);
    drop(wbuf);
    status
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

/// The NT path a handle-based delete/rename should act on, and whether finding
/// it required consulting the OS about the handle's *current* target (the same
/// provenance bit [`DecodedPath`] carries, for the same reason: a
/// `RootMap`-backed answer computed from it must not be cached under it).
///
/// **The third source is what closes an escape.** `PATH_TABLE` is populated by
/// `record_path`, which runs only on an open *this shim intercepted*. A handle
/// inherited across `CreateProcess`, duplicated in from another process, or
/// opened before injection is in no table of ours — and `setinfo_hook`'s
/// non-synthetic branch used to read that miss as "nothing to do" and hand the
/// call to the real `NtSetInformationFile`. For a **delete** that unlinked the
/// real file under a managed root, which is the same breach `delete_hook`
/// exists to prevent, arriving by a different door. There is no cheap
/// backstop for it either: a `PATH_TABLE` miss leaves no path at all, so there
/// is nothing to apply `path_is_ours` to until one is recovered.
///
/// So ask the OS, exactly as `parent_dir_of_handle`'s case 4 already does for
/// `OBJECT_ATTRIBUTES.RootDirectory` — `GetFinalPathNameByHandleW` needs no
/// reopen, since the caller is handing us a handle it currently holds.
///
/// **The order is correctness, not only cost.** `PATH_TABLE` holds the path the
/// caller *named*, and for a handle that came off `create_hook`'s
/// `Decision::Redirect` arm that is the virtual path while the handle itself
/// targets the overlay copy. Asking the OS first would hand back the overlay
/// file's own location, which resolves under no managed root, so the whiteout
/// would be skipped and the operation would go to the kernel — reintroducing the
/// escape from the other end. The recorded name wins wherever there is one:
///
/// 1. `PATH_TABLE` — an intercepted open whose path was under a managed root.
/// 2. `HANDLE_PATHS` — every other intercepted open. A handle here but not in
///    (1) is one `record_path` declined, i.e. outside every root, so this
///    answers the common "delete a file that is none of our business" case
///    without touching the OS.
/// 3. `GetFinalPathNameByHandleW`. Only reached for a handle the shim never saw
///    opened, and only on a delete/rename set-info, which is rare — this is not
///    a per-call cost on any hot path.
///
/// # Safety
/// `handle` must be the live handle of an in-flight `NtSetInformationFile` this
/// process is making, which is what `final_path_for_handle` requires. This
/// neither closes it nor takes ownership of it.
unsafe fn setinfo_source_path(handle: HANDLE) -> Option<(String, bool)> {
    if let Ok(t) = PATH_TABLE.lock() {
        if let Some(p) = t.get(&(handle as isize)) {
            return Some((p.clone(), false));
        }
    }
    if let Some(p) = path_of_handle(handle) {
        return Some((p, false));
    }
    vfs_win::final_path_for_handle(handle).map(|p| (p, true))
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
/// but the path reads as gone/moved.
///
/// Two things sit on top of that, both from gate 5's Task 5, and each has its
/// own comment at the check itself:
///
/// - **The source is resolved even when no table knows the handle**
///   (`setinfo_source_path`). A `PATH_TABLE` miss used to mean "not ours", and
///   for an inherited or pre-injection handle on an under-root path that sent
///   a delete to the real file.
/// - **A refusal keyed on the *target*, not the source.** A rename whose
///   destination lands under a managed root is refused even when the source is
///   outside every one of them and no source-keyed arm ever looked at it.
///
/// Between them the rule is one sentence: a rename either has both sides under
/// the same root, and is routed, or it touches no root at all, and passes
/// through. Everything else is refused, and a delete of an under-root path that
/// nothing here absorbed is refused with it rather than reaching the kernel.
/// `FileCompletionInformation` — binds a handle to an I/O completion port.
const FILE_COMPLETION_INFORMATION: u32 = 30;

unsafe fn setinfo_hook_body(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::SetInfo);
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
            if let (Some((fh, _, _, _, _)), Some(c)) = (
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
                if let Some((root, vpath)) = c.vpath_under_root(&nt) {
                    let src = if vpath.is_empty() { ".".to_string() } else { vpath };
                    let ok = if is_delete {
                        c.delete(root, &src).is_ok()
                    } else {
                        match parse_rename_target(info, length)
                            .and_then(|t| c.vpath_under_root(&t))
                        {
                            // A rename whose target lands under a *different*
                            // root is refused rather than guessed at: the
                            // wire carries one root for both sides, and the
                            // provider contract has no cross-root move.
                            //
                            // It does **not** fall through — an earlier
                            // version of this comment claimed it did, and
                            // `Engine::rename` was written to match that
                            // description, which is how the engine-side
                            // branch below ended up handing cross-root moves
                            // to the real filesystem. What actually happens is
                            // `ok = false` and `STATUS_UNSUCCESSFUL` twelve
                            // lines down, the same as any other refused
                            // delete/rename on a virtual handle. The engine
                            // branch now fails closed the same way.
                            Some((dst_root, dstv)) if dst_root == root => {
                                let dst = if dstv.is_empty() { ".".to_string() } else { dstv };
                                c.rename(root, &src, &dst).is_ok()
                            }
                            _ => false,
                        }
                    };
                    if ok {
                        setinfo_ok_iosb(iosb);
                        return STATUS_SUCCESS;
                    }
                    return STATUS_UNSUCCESSFUL;
                }
                // is_delete/is_rename matched the class but the handle's path
                // or vpath could not be resolved — falls through to the soft
                // no-op below rather than a hard failure. Still worth logging:
                // it means a delete/rename was silently swallowed.
            }
        }
        // Everything else lands here: a class we deliberately never act on
        // (or a delete/rename we recognized but could not route). Silent
        // success here for a class we actually needed to handle is exactly
        // the bug this counter exists to make discoverable — see
        // `hookstats::note_setinfo_noop`.
        crate::hookstats::note_setinfo_noop(class);
        return STATUS_SUCCESS;
    }
    let is_delete = is_delete_request(info, length, class);
    let is_rename = matches!(class, FILE_RENAME_INFORMATION | FILE_RENAME_INFORMATION_EX);

    if is_delete || is_rename {
        // Not just `PATH_TABLE`: a handle the shim never saw opened has no
        // entry there, and reading that miss as "not ours" is what let a
        // delete on an inherited or pre-injection under-root handle reach the
        // real file. See `setinfo_source_path`.
        let source = setinfo_source_path(handle);
        // Held for every `RootMap`-backed question asked with an OS-consulted
        // source path below (`Engine::whiteout`/`rename`, `path_is_ours`) —
        // that string is a fact about the handle's target right now, not a
        // pure function of its own bytes. See `vfs_redirect::UncachedScope`.
        let _uncached_guard = source
            .as_ref()
            .is_some_and(|(_, os_consulted)| *os_consulted)
            .then(vfs_redirect::UncachedScope::enter);
        let nt = source.map(|(p, _)| p);
        if let (Some(nt), Some(engine)) = (nt, ENGINE.get()) {
            let handled = if is_delete {
                engine.whiteout(&nt)
            } else {
                match parse_rename_target(info, length) {
                    Some(target) => match engine.rename(&nt, &target) {
                        crate::engine::RenameOutcome::Handled => true,
                        // Both sides under managed roots, but different ones.
                        // Trampolining here is what let the kernel physically
                        // move an overlay-captured file out onto real disk
                        // under the destination root — where it then reads
                        // back as missing, because that root seals anything
                        // the provider graph does not serve. Fail closed, with
                        // the same status the FUSE-handle branch above already
                        // returns for the identical case.
                        crate::engine::RenameOutcome::CrossRoot => {
                            return STATUS_UNSUCCESSFUL
                        }
                        crate::engine::RenameOutcome::Declined => false,
                    },
                    None => false,
                }
            };
            if handled {
                // Suppress the real delete/rename; report success to the caller.
                setinfo_ok_iosb(iosb);
                return STATUS_SUCCESS;
            }
            // **The source is under a managed root and nothing above absorbed
            // the operation.** `tramp` below would hand it to the kernel,
            // which acts on the real file — and this arm is reached by three
            // routes that all end that way:
            //
            //  - A **delete** that `Engine::whiteout` declined (no overlay
            //    configured, or a path resolving with an empty remainder).
            //    The path-based `delete_hook` has had a `path_is_ours`
            //    backstop for exactly this since it was written; leaving its
            //    sibling fail-open is the same divergence, and the next reader
            //    would have had two deletes to copy from and no way to tell
            //    which was right.
            //  - A **rename out** of a managed root to a target outside every
            //    one of them. `Engine::rename` answers `Declined` (its `to`
            //    side resolves nowhere) and the kernel then performs the move,
            //    which *unlinks a real file under a managed root*. That the
            //    destination is legitimately outside does not make the source
            //    side any less of a breach, and it is the same one the
            //    target-keyed check below closes in the other direction.
            //  - A **rename whose target cannot be parsed at all**
            //    (`parse_rename_target` -> `None`, e.g. a target named against
            //    a directory handle we cannot resolve). An operation on an
            //    under-root path whose other half we cannot even read is the
            //    last thing that should be forwarded blind.
            //
            // The rule this leaves is one sentence: a rename either has both
            // sides under the same root, and is routed, or it does not touch a
            // root at all, and is trampolined. Everything between is refused.
            if path_is_ours(&nt) {
                return STATUS_ACCESS_DENIED;
            }
        }
        // **A rename whose *target* lands under a managed root** (gate 5,
        // Task 5). Everything above is keyed on the *source*, and for a source
        // outside every root none of it runs: `record_path` inserts into
        // `PATH_TABLE` only when `path_is_ours(path)`, so an outside handle is
        // never recorded, the engine arm above is skipped, and `tramp` below
        // performed the move — physically creating a file under the
        // destination root, where it then read back as missing because that
        // root seals every path the provider graph does not serve.
        //
        // The destination is what decides containment. Content crossing *into*
        // the VFS by a route the director never saw is the same failure as
        // content crossing out of it, and the source being legitimately
        // outside does not make the target's root any less managed.
        //
        // Refused rather than routed, and there is no third option available:
        // `OP_RENAME` carries **one** root and two vpaths under it (see
        // `FuseClient::rename`), so the provider contract has no operation for
        // an import from outside. `STATUS_ACCESS_DENIED` rather than the
        // `STATUS_UNSUCCESSFUL` the cross-root arm above returns, because it is
        // a different answer: cross-root is "the graph cannot express this
        // move", this is "the destination will not accept content by this
        // route at all".
        //
        // NOTE: `parse_rename_target` discards `parent_dir_of_handle`'s
        // OS-consulted provenance bit, so a target named against a directory
        // handle the shim never saw opened reaches `path_is_ours` here without
        // an `UncachedScope`. That is the known gap `parse_rename_target`
        // already records for `engine.rename`, not a new one — it is listed
        // there rather than fixed here so both callers are fixed at once.
        if is_rename {
            if let Some(target) = parse_rename_target(info, length) {
                if path_is_ours(&target) {
                    return STATUS_ACCESS_DENIED;
                }
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
    let Some((fh, size, is_dir, pos, _append_only)) = crate::fuse_synth::lookup(handle as isize)
    else {
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
            (*bi).creation_time = SYNTH_FILETIME;
            (*bi).last_access_time = SYNTH_FILETIME;
            (*bi).last_write_time = SYNTH_FILETIME;
            (*bi).change_time = SYNTH_FILETIME;
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
            (*ni).creation_time = SYNTH_FILETIME;
            (*ni).last_access_time = SYNTH_FILETIME;
            (*ni).last_write_time = SYNTH_FILETIME;
            (*ni).change_time = SYNTH_FILETIME;
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

/// `NtQueryVolumeInformationFile` hook — `GetFileType` needs
/// `FileFsDeviceInformation` on synthetic handles.
unsafe fn qvol_hook_body(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QVol);
    let tramp = match TRAMP_QVOL {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
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

/// Whether `handle` is a synthetic handle **that is currently open**.
///
/// `is_fuse_synth` alone is a bit-47 tag test, and that is not the same
/// question. `INVALID_HANDLE_VALUE` is `-1` — every bit set, including bit 47
/// — so it passes the tag test, and so does any synthetic handle that has
/// already been closed or was never issued. Answering `STATUS_SUCCESS` for
/// those turns a caller's error into a silent one: a lock on a handle whose
/// open actually failed would appear to be held.
///
/// Every other synthetic branch in this file resolves the handle before acting
/// (`read_hook`, `fuse_query_information`), which is why this exists rather
/// than the bare tag test the lock trio first shipped with.
fn open_synth(handle: HANDLE) -> bool {
    crate::fuse_synth::is_fuse_synth(handle as isize)
        && crate::fuse_synth::lookup(handle as isize).is_some()
}

/// The NT path a synthetic handle was opened as, for the lock counters.
/// `None` for a handle no under-root open recorded.
fn synth_path(handle: HANDLE) -> Option<String> {
    match PATH_TABLE.lock() {
        Ok(t) => t.get(&(handle as isize)).cloned(),
        Err(_) => None,
    }
}

/// `NtLockFile` hook — grants byte-range locks on synthetic handles locally.
///
/// **Why this exists.** A synthetic handle is a tagged value in `fuse_synth`'s
/// table, not a kernel file object, so any NT call without a detour hands that
/// value to the real kernel and gets `STATUS_INVALID_HANDLE` back. Measured
/// 2026-08-14: `GetPrivateProfileStringW` — how Skyrim loads `SkyrimPrefs.ini`
/// — issues `NtOpenFile → NtLockFile → NtQueryInformationFile → NtReadFile →
/// NtUnlockFile → NtClose`, and with `NtLockFile` unhooked the sequence
/// stopped dead at step 2. The API then returned the *caller's default* for
/// every key, so the game received no INI data at all — not stale data, not
/// real-disk data. `WritePrivateProfileStringW` failed the same way one
/// operation earlier. Neither showed up as a read or write at the director;
/// both showed up as an open and nothing else.
///
/// **The deliberate semantic gap.** This grants a lock that does not exist.
/// Nothing is recorded, nothing conflicts, and two callers asking for the same
/// exclusive byte range both get `STATUS_SUCCESS`. That is chosen, not
/// overlooked:
///
/// - Inside a sealed managed root the director is the only route to the bytes,
///   and there is no cross-process byte-range locking anywhere in the design
///   today — so there is no lock table for a real answer to consult.
/// - Refusing instead (`STATUS_LOCK_NOT_GRANTED`) would leave the profile APIs
///   exactly as broken as an unhooked call did; it swaps a wrong status for a
///   different wrong status.
///
/// **Do not read that as "there is only one writer".** There is not, by
/// design: `cpiw_hook` propagates injection into child processes, so a
/// launcher and a game — or a game and a mod manager's helper — are routinely
/// in one session. And the API that exposed this bug is the worst case for a
/// fake lock: `WritePrivateProfileString` is a read-modify-write, and the lock
/// it takes here is exactly what stops two of those from losing each other's
/// updates. Two injected writers on one INI will both be granted the same
/// exclusive range and one update will disappear.
///
/// That is a real hole, not a theoretical one; it is accepted because the
/// alternative on offer was every INI staying unreadable, not because it is
/// harmless. Closing it needs a byte-range table in the director — the only
/// component both processes share. Until then
/// `hookstats::note_synthetic_lock` counts every grant by path, so the
/// contention shows up in a report instead of only in corrupted settings.
///
/// **Which handles this answers.** Only ones [`open_synth`] resolves. The
/// bit-47 tag test alone would also catch `INVALID_HANDLE_VALUE` and any
/// closed or never-issued synthetic handle, and answering `STATUS_SUCCESS` for
/// those would report a lock held on a file the caller never opened.
///
/// **Completion.** Answered synchronously: `STATUS_SUCCESS`, a completed
/// `IO_STATUS_BLOCK`, and `SetEvent` if the caller supplied one — the same
/// shape `read_hook` uses, including its one limitation, that we do not run
/// the caller's APC. That limitation is counted rather than assumed away:
/// `note_read_completion` classifies every synthetic lock by the completion
/// its caller expected, so an APC-supplied lock — the shape that would wait
/// forever on a callback we never make — shows up in the report's async
/// section instead of passing for an ordinary grant. `FailImmediately` needs
/// no branch: `false` means the caller is willing to block for the lock, and
/// an immediate grant satisfies that strictly better than waiting.
#[allow(clippy::too_many_arguments)]
unsafe fn lock_hook_body(
    handle: HANDLE,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    byte_offset: *const i64,
    length: *const i64,
    key: u32,
    fail_immediately: u8,
    exclusive: u8,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Lock);
    let tramp = match TRAMP_LOCK {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        if !open_synth(handle) {
            return STATUS_INVALID_HANDLE;
        }
        // Classified the same way `read_hook`/`write_hook` classify theirs: an
        // APC-supplied lock is a completion we accept and never deliver, and
        // that is the one caller shape here that can actually hang. Counting
        // it is what makes it visible in the async section instead of looking
        // like an ordinary synchronous grant.
        crate::hookstats::note_read_completion(!apc.is_null(), !event.is_null());
        crate::hookstats::note_synthetic_lock(
            if exclusive != 0 { "lock-exclusive" } else { "lock-shared" },
            synth_path(handle).as_deref(),
        );
        synth_iosb_ok(iosb, 0);
        if !event.is_null() {
            windows_sys::Win32::System::Threading::SetEvent(event);
        }
        return STATUS_SUCCESS;
    }
    tramp(handle, event, apc, apc_ctx, iosb, byte_offset, length, key, fail_immediately, exclusive)
}

/// `NtUnlockFile` hook — the release half of [`lock_hook`], and success for
/// the same reason: a lock that was never recorded cannot fail to be released.
unsafe fn unlock_hook_body(
    handle: HANDLE,
    iosb: *mut c_void,
    byte_offset: *const i64,
    length: *const i64,
    key: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Unlock);
    let tramp = match TRAMP_UNLOCK {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        if !open_synth(handle) {
            return STATUS_INVALID_HANDLE;
        }
        crate::hookstats::note_synthetic_lock("unlock", synth_path(handle).as_deref());
        synth_iosb_ok(iosb, 0);
        return STATUS_SUCCESS;
    }
    tramp(handle, iosb, byte_offset, length, key)
}

/// `NtFlushBuffersFile` hook. Success on a synthetic handle: the director owns
/// durability for everything behind one, and there is no user-mode buffer here
/// to push — `write_hook` forwards each write over the ring as it happens.
///
/// Unlike the lock pair this is not a lie about state, but it is still weaker
/// than what the caller asked for: it promises the bytes are durable, and what
/// it can actually guarantee is that they reached the director.
unsafe fn flush_hook_body(handle: HANDLE, iosb: *mut c_void) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::FlushBuffers);
    let tramp = match TRAMP_FLUSH {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        if !open_synth(handle) {
            return STATUS_INVALID_HANDLE;
        }
        crate::hookstats::note_synthetic_lock("flush", synth_path(handle).as_deref());
        synth_iosb_ok(iosb, 0);
        return STATUS_SUCCESS;
    }
    tramp(handle, iosb)
}

/// `NtQueryInformationFile` hook. Spoofs the two name classes —
/// `FileNameInformation` (9) and `FileNormalizedNameInformation` (48) — on a
/// redirected handle -> the virtual path, so `GetFinalPathNameByHandleW`
/// reports where the mod file appears to live. Everything else passes through.
///
/// # Why class 9 is spoofed too, having once been documented as unspoofable
///
/// This comment used to read "spoofing class 9 breaks
/// `GetFinalPathNameByHandleW`", and that was a true measurement of the shim as
/// it then stood — but the cause was consistency, not class 9 itself.
/// `GetFinalPathNameByHandleW` builds its answer from three sources and treats
/// them as describing one file:
///
/// 1. `NtQueryObject(ObjectNameInformation)` — the full NT name;
/// 2. `NtQueryInformationFile(FileNameInformation)` (class 9) — used for its
///    **length only**: the device prefix is taken to be
///    `ObjectName[.. ObjectName.len - class9.len]`;
/// 3. `NtQueryInformationFile(FileNormalizedNameInformation)` (class 48) —
///    appended to the drive letter that prefix maps to.
///
/// Spoof any one of those and the subtraction in (2) slices at the wrong
/// offset. Measured 2026-09-01 with class 1 spoofed and class 9 left truthful:
/// ObjectName `\Device\HarddiskVolume3\vfstmp\vfs-diag\mod.esp` (53 chars)
/// minus the backing file's class 9 `\vfstmp\vfs-diag-backing\backing_blob.dat`
/// (47 chars) gave a 6-character "device" of `\Devic`, which maps to no drive,
/// so the call failed with `ERROR_FILE_NOT_FOUND`.
///
/// The rule is therefore **all three or none**: classes 1, 9 and 48 must
/// describe the same path. They now do, and the subtraction lands on the real
/// device prefix again because both operands moved by the same amount.
unsafe fn qif_hook_body(
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
    if (class == FILE_NORMALIZED_NAME_INFORMATION || class == FILE_NAME_INFORMATION)
        && !info.is_null()
    {
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

/// Resolve a DOS drive spec (`"C:"`) to the host's device path for it.
///
/// Separate from [`spoofed_object_name`] so that function stays pure and
/// testable: this is the only part that has to ask the running kernel.
fn device_for_drive(drive: &str) -> Option<String> {
    let name: Vec<u16> = drive.encode_utf16().chain(core::iter::once(0)).collect();
    let mut out = [0u16; 512];
    // SAFETY: `name` is NUL-terminated and `out` is writable for its own length,
    // which is what `QueryDosDeviceW` requires. It reports 0 on failure.
    #[allow(unsafe_code)]
    let n = unsafe {
        windows_sys::Win32::Storage::FileSystem::QueryDosDeviceW(
            name.as_ptr(),
            out.as_mut_ptr(),
            out.len() as u32,
        )
    };
    if n == 0 {
        return None;
    }
    let end = out.iter().position(|&c| c == 0).unwrap_or(n as usize);
    if end == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&out[..end]))
}

/// The `ObjectNameInformation` name to emit for a redirected handle, given the
/// host's own answer for the same handle (`real`) and the virtual NT path the
/// caller opened (`vpath`).
///
/// The host's answer is consulted only for its **prefix convention** — never
/// for its content, which is exactly the backing path being hidden. `device_of`
/// is consulted only on the `\Device\` branch, so a host that uses `\??\` pays
/// no `QueryDosDeviceW` call.
///
/// `None` means "emit nothing, pass the host's answer through": a convention
/// this function does not recognise, or a virtual path that is not a
/// drive-letter path, is a case where a made-up name would be worse than the
/// real one.
fn spoofed_object_name(
    real: &str,
    vpath: &str,
    device_of: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    // DOS portion of the virtual path: `C:\dir\file`, no NT prefix.
    let dos = vpath
        .strip_prefix(r"\??\")
        .or_else(|| vpath.strip_prefix(r"\\?\"))
        .unwrap_or(vpath);
    let b = dos.as_bytes();
    // Must be `X:` optionally followed by a rooted remainder. Anything else
    // (a UNC name, a volume GUID, a relative leftover) has no drive letter to
    // resolve and no safe device form to build.
    if b.len() < 2 || !b[0].is_ascii_alphabetic() || b[1] != b':' {
        return None;
    }
    if b.len() > 2 && b[2] != b'\\' {
        return None;
    }
    if real.starts_with(r"\??\") {
        Some(format!(r"\??\{dos}"))
    } else if real.starts_with(r"\Device\") {
        let dev = device_of(&dos[..2])?;
        Some(format!("{dev}{}", &dos[2..]))
    } else {
        None
    }
}

/// `NtQueryObject` hook. Answers `ObjectNameInformation` (class 1) for a handle
/// the shim redirected -> the VIRTUAL path, in the prefix convention this host
/// actually uses. Every other class, and every handle we do not track, passes
/// through untouched: this API answers about events, mutexes, sections and
/// registry keys too, and inventing a name for one of those would break
/// unrelated Windows APIs.
///
/// Why the convention is discovered rather than assumed: measured 2026-09-01,
/// Windows returns `\Device\HarddiskVolume3\...` while Wine returns `\??\C:\...`,
/// and `QueryDosDeviceW("C:")` reports `\Device\HarddiskVolume1` on Wine — it
/// disagrees with Wine's own `NtQueryObject`. So building a device path from it
/// would emit a form Wine never produces. Instead the trampoline runs first and
/// its answer's prefix is reused.
///
/// **This closes a pre-existing leak on Windows, not only on Wine.**
/// `GetFinalPathNameByHandleW` happens to route through
/// `NtQueryInformationFile(FileNormalizedNameInformation)` on Windows — hooked
/// by [`qif_hook_body`] — so the leak hid there; any caller reaching
/// `NtQueryObject` directly got the backing path, silently. Wine routes
/// `GetFinalPathNameByHandleW` through this entry point instead, which is how
/// the leak became visible at all.
///
/// # The too-small-buffer contract (measured 2026-09-01, both hosts)
///
/// A caller that size-probes — query with a tiny buffer, allocate what
/// `ReturnLength` asks for, query again — loops or fails unless this is
/// reproduced exactly. Both hosts agreed, which is why one rule serves both:
///
/// | `ObjectInformationLength` | Windows 11 | Wine 11.0 (GE-Proton11-6) |
/// |---|---|---|
/// | 0 | `STATUS_INFO_LENGTH_MISMATCH` (0xC0000004) | same |
/// | 8 (< the 16-byte header) | `STATUS_INFO_LENGTH_MISMATCH` | same |
/// | 16 (header only) | `STATUS_BUFFER_OVERFLOW` (0x80000005) | same |
/// | `required - 1` | `STATUS_BUFFER_OVERFLOW` | same |
///
/// `ReturnLength` was set to the full required size in **every** one of those
/// cases, including length 0, and a NULL `ReturnLength` was tolerated rather
/// than faulted. `required` = 16 + name bytes + 2 for the NUL; `Length`
/// excludes the NUL, `MaximumLength` includes it, and `Buffer` points 16 bytes
/// into the caller's own buffer on both hosts.
unsafe fn qobj_hook_body(
    handle: HANDLE,
    class: u32,
    info: *mut c_void,
    length: u32,
    ret_len: *mut u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QObj);
    let tramp = match TRAMP_QOBJ {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // Cheapest rejections first, in order of how much of the world they let
    // past untouched. A class we do not answer is most of the traffic.
    if class != OBJECT_NAME_INFORMATION {
        return tramp(handle, class, info, length, ret_len);
    }
    // An untracked handle must cost nothing but this map lookup — no
    // allocation, no scratch call. It may be an event, a mutex, a section or a
    // registry key, and we have nothing true to say about any of them.
    let vpath = match PATH_TABLE.lock() {
        Ok(t) => t.get(&(handle as isize)).cloned(),
        Err(_) => None,
    };
    let Some(vpath) = vpath else {
        return tramp(handle, class, info, length, ret_len);
    };

    // The host's own answer, for its prefix convention. Sized generously so
    // the common case is one call; grown once if some path is longer than that.
    // Note this is deliberately NOT served from a fuse-synthetic handle — those
    // are not kernel objects, the trampoline fails on them, and we fall through
    // to letting the host answer rather than guessing a convention.
    let mut scratch = vec![0u8; 2048];
    let mut need: u32 = 0;
    let mut st = tramp(
        handle,
        class,
        scratch.as_mut_ptr().cast(),
        scratch.len() as u32,
        &mut need,
    );
    if (st == STATUS_BUFFER_OVERFLOW || st == STATUS_INFO_LENGTH_MISMATCH)
        && need as usize > scratch.len()
    {
        scratch = vec![0u8; need as usize];
        st = tramp(
            handle,
            class,
            scratch.as_mut_ptr().cast(),
            scratch.len() as u32,
            &mut need,
        );
    }
    if st < 0 {
        // A real failure — an unnamed object, a revoked handle, a synthetic
        // handle. Let the host answer the caller directly rather than
        // substituting a success it did not earn.
        return tramp(handle, class, info, length, ret_len);
    }
    let real = {
        // SAFETY: the trampoline reported success into `scratch`, so its first
        // `OBJECT_NAME_INFORMATION_HEADER` bytes are a UNICODE_STRING whose
        // `Length` bytes of name follow. `Buffer` is ignored deliberately: the
        // name is read at the fixed offset both hosts were measured to use, so
        // a bogus pointer cannot be followed.
        let hdr = OBJECT_NAME_INFORMATION_HEADER;
        let n = u16::from_le_bytes([scratch[0], scratch[1]]) as usize;
        if n == 0 || !n.is_multiple_of(2) || hdr + n > scratch.len() {
            return tramp(handle, class, info, length, ret_len);
        }
        let units: Vec<u16> = scratch[hdr..hdr + n]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        String::from_utf16_lossy(&units)
    };

    let Some(name) = spoofed_object_name(&real, &vpath, device_for_drive) else {
        return tramp(handle, class, info, length, ret_len);
    };

    let name16: Vec<u16> = name.encode_utf16().collect();
    let name_bytes = name16.len() * 2;
    // `UNICODE_STRING::MaximumLength` is a u16 and must cover the NUL. A name
    // that cannot be described in that field is one we must not try to emit.
    if name_bytes + 2 > u16::MAX as usize {
        return tramp(handle, class, info, length, ret_len);
    }
    let required = OBJECT_NAME_INFORMATION_HEADER + name_bytes + 2;
    // Set unconditionally and before any short-buffer return: both hosts fill
    // `ReturnLength` even when they write nothing at all.
    if !ret_len.is_null() {
        core::ptr::write_unaligned(ret_len, required as u32);
    }
    if info.is_null() || (length as usize) < OBJECT_NAME_INFORMATION_HEADER {
        return STATUS_INFO_LENGTH_MISMATCH;
    }
    if (length as usize) < required {
        return STATUS_BUFFER_OVERFLOW;
    }
    // SAFETY: `info` is non-null and the caller declared `length` writable
    // bytes, and `length >= required` was just checked, so every write below
    // lands inside the caller's buffer.
    #[allow(unsafe_code)]
    unsafe {
        let p = info as *mut u8;
        core::ptr::write_unaligned(p as *mut u16, name_bytes as u16);
        core::ptr::write_unaligned(p.add(2) as *mut u16, (name_bytes + 2) as u16);
        // Both hosts point `Buffer` at the caller's own buffer, 16 bytes in.
        core::ptr::write_unaligned(
            p.add(8) as *mut usize,
            p.add(OBJECT_NAME_INFORMATION_HEADER) as usize,
        );
        let dst = p.add(OBJECT_NAME_INFORMATION_HEADER);
        for (i, u) in name16.iter().enumerate() {
            core::ptr::write_unaligned(dst.add(i * 2) as *mut u16, *u);
        }
        core::ptr::write_unaligned(dst.add(name_bytes) as *mut u16, 0u16);
    }
    STATUS_SUCCESS
}

/// `NtWriteFile` hook. For synthetic (fuse) write handles, forward the game's
/// buffer to the JVM overlay over the ring and complete the IRP; real handles
/// pass straight through. `ByteOffset` NULL / negative sentinel = current pos.
#[allow(clippy::too_many_arguments)]
unsafe fn write_hook_body(
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
        if let Some((fh, size, _is_dir, pos, append_only)) =
            crate::fuse_synth::lookup(handle as isize)
        {
            // Append-only access (FILE_APPEND_DATA without FILE_WRITE_DATA)
            // forces every write to the current end of file at the kernel
            // level, ignoring any offset the caller supplies — a real handle
            // enforces this itself; ours has to do it here.
            let off = if append_only { pos } else { explicit.unwrap_or(pos) };
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
            // Append-only always tracks position (every write moved EOF
            // forward regardless of what the caller passed); otherwise only
            // an implicit-offset write consumes the file pointer.
            if append_only || explicit.is_none() {
                crate::fuse_synth::set_position(handle as isize, off + n as u64);
            }
            // The synthetic size was set once at open and never touched
            // since — a write that extends the file must bump it too, or
            // `read_hook`'s EOF check and `fuse_query_information`'s
            // `metadata().len()` keep reporting the pre-write length forever.
            // Only reachable now that writes actually reach the director
            // instead of falling through to a real file (whose kernel FCB
            // would have tracked this for free).
            let end = off + n as u64;
            if end > size {
                crate::fuse_synth::set_size(handle as isize, end);
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

/// `NtReadFile` hook. Synthetic (fuse) handles are answered from the director
/// over the ring; real handles pass straight through. `ByteOffset` of NULL or
/// the "use current position" sentinel (-1/-2) means "current position".
///
/// This doc comment used to describe a second synthetic case — copying bytes
/// out of a mapped zip window — and used to sit, orphaned, above `write_hook`.
/// Gate 4 task 7 deleted that case with the rest of the zip-window server.
#[allow(clippy::too_many_arguments)]
unsafe fn read_hook_body(
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
        if let Some((fh, size, _is_dir, pos, _append_only)) =
            crate::fuse_synth::lookup(handle as isize)
        {
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
    tramp(handle, event, apc, apc_ctx, iosb, buffer, length, byte_offset, key)
}

/// Back a VFS-served PE with a real file so the kernel can build the image
/// section, and return the trampoline's status.
///
/// `None` means the backing file could not be produced, and the caller should
/// fall back to the manual mapper.
///
/// The cache is keyed on the vpath and size, so an assembly loaded repeatedly
/// materialises once. Files live under the shim's own temp directory and are
/// left for the OS to reclaim; they are content, not secrets, and the process
/// may still have sections open on them at exit.
#[allow(clippy::too_many_arguments)]
unsafe fn real_image_section(
    pe: &[u8],
    file_handle: HANDLE,
    section_handle: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    max_size: *mut i64,
    page_prot: u32,
    alloc_attrs: u32,
    tramp: NtCreateSectionFn,
) -> Option<NTSTATUS> {
    use std::os::windows::io::AsRawHandle;

    // Our own file I/O must not re-enter the hooks that brought us here.
    let _io = ShimIoGuard::enter();

    let vpath = crate::fuse_synth::abs_path(file_handle as isize)?;
    // FNV-1a over the vpath and length: stable across runs, and distinct for
    // two builds of the same assembly.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in vpath.to_ascii_lowercase().bytes().chain(pe.len().to_le_bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    let dir = std::env::temp_dir().join("vfs-pe-cache");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{h:016x}.bin"));

    // Write once. A concurrent writer would be writing identical bytes, but a
    // reader must never see a half-written image, so build beside it and rename.
    let good = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).ok() == Some(pe.len() as u64);
    if !good(&path) {
        let tmp = dir.join(format!("{h:016x}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, pe).ok()?;
        // Rename is atomic within a directory; an existing good file wins.
        if std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            if !good(&path) {
                return None;
            }
        }
    }

    // GENERIC_READ | GENERIC_EXECUTE. An *image* section requires execute
    // access on the backing file; a plain `File::open` grants only read and
    // `NtCreateSection` answers STATUS_ACCESS_DENIED (0xC0000022).
    use std::os::windows::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .access_mode(0x8000_0000 | 0x2000_0000)
        .share_mode(0x0000_0001 | 0x0000_0002) // FILE_SHARE_READ | FILE_SHARE_WRITE
        .open(&path)
        .ok()?;
    let st = tramp(
        section_handle,
        access,
        oa,
        max_size,
        page_prot,
        alloc_attrs,
        f.as_raw_handle() as HANDLE,
    );
    // The section holds its own reference to the file object, so closing ours
    // here (on drop) does not disturb it.
    if st < 0 {
        return None;
    }
    Some(st)
}

/// Map a FUSE synthetic file into a synthetic section.
///
/// - **SEC_IMAGE**: map PE from director bytes (rare; PEs usually host-tramped).
/// - **Data ≤256 MiB**: eager stream into a private mapping (primary stack is
///   expanded to 16 MiB by vfs-inject — matches the known-good director-only path).
/// - **Data >256 MiB**: lazy demand-page (reserve + warm + VEH) so multi‑GiB BSAs
///   never full-preload.
#[allow(clippy::too_many_arguments)]
unsafe fn fuse_create_section(
    section_handle: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    max_size: *mut i64,
    page_prot: u32,
    alloc_attrs: u32,
    file_handle: HANDLE,
    tramp: NtCreateSectionFn,
) -> NTSTATUS {
    let _page_prot = page_prot;
    let Some((fh, size, is_dir, _, _)) = crate::fuse_synth::lookup(file_handle as isize) else {
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

        // Preferred path: write the bytes to a real file and let the kernel
        // build the image section.
        //
        // The manual mapper below reimplements what Windows does when it maps
        // a PE, and it is only ever approximately right: one `VirtualAlloc` of
        // `PAGE_EXECUTE_READWRITE` for the whole image, no per-section
        // protections, and a single shared region where a real image section
        // gives each view its own copy-on-write. A .NET application maps
        // hundreds of assemblies and the CLR faulted inside its own code
        // (`c0000005`) on that difference. A genuine section gets all of it
        // from the kernel for the price of one cached file on disk.
        if let Some(st) = real_image_section(
            &pe,
            file_handle,
            section_handle,
            access,
            oa,
            max_size,
            page_prot,
            alloc_attrs,
            tramp,
        ) {
            return st;
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
                None => {
                    STATUS_INVALID_FILE_FOR_SECTION
                }
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

/// `NtCreateSection` hook: a FUSE synthetic file handle becomes a synthetic
/// section (lazy data section, or an eagerly mapped PE for `SEC_IMAGE`) via
/// [`fuse_create_section`]. Every other handle passes through — including, as
/// of gate 4 task 7, the zip-window synthetic file handles this hook used to
/// also answer for, which no longer exist.
unsafe fn create_section_hook_body(
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
            access,
            oa,
            max_size,
            page_prot,
            alloc_attrs,
            file_handle,
            tramp,
        );
    }
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

/// `NtMapViewOfSection` hook: synthetic sections return a pointer into the
/// region the shim already mapped for them. Real sections pass through.
#[allow(clippy::too_many_arguments)]
unsafe fn map_view_hook_body(
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

/// `NtUnmapViewOfSection` hook: synthetic views are bookkeeping-only. Dropping
/// the last reference to one does not tear the memory down here — the region
/// belongs to whoever mapped it (see `lazy_section::on_section_closed`).
unsafe fn unmap_view_hook_body(process: HANDLE, base: *mut c_void) -> NTSTATUS {
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
unsafe fn cpiw_hook_body(
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

#[allow(clippy::too_many_arguments)]
unsafe fn qdirex_hook_body(
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
    )
}

/// The classic entry point. Same body, different argument shape.
#[allow(clippy::too_many_arguments)]
unsafe fn qdir_hook_body(
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
    )
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
                        crate::hookstats::ReadDirSource::Os,
                    );
                }
                return passthrough();
            }
            Some(t) => (restart || t.state.is_none(), t.dir_nt_path.clone()),
        }
    };

    // Phase 2 (unlocked): build the listing. The handle only reached
    // `DIR_TABLE` because `tag_under_root` found `path_is_ours` true for it,
    // so *every* listing built here is a listing under a managed root — and
    // the governing invariant says the real filesystem beneath a managed root
    // is unreachable by any spelling. A directory listing is a spelling. So
    // there are exactly two things that may appear in one:
    //
    // 1. What the director serves. When the FUSE client recognises the
    //    directory its `readdir` is the whole answer, authoritative and
    //    unmerged.
    // 2. Failing that, the shim-local write overlay's own entries — content
    //    this process created through gate 4's write path, which physically
    //    lives outside the root and which the director may not know about.
    //
    // What may **not** appear is the real directory behind the mount. Until
    // gate 4 task 8b this function had a third branch that drained exactly
    // that (`drain_real` over the handle) whenever the client was absent or
    // did not recognise the path, and put the overlay on top of it — so a
    // real, unserved file under a managed root would be listed. Reads,
    // metadata and writes were each sealed and proven by the escape matrix;
    // enumeration was only ever *argued* to follow from read-open containment,
    // and it does not follow: separate predicates, and no test on either side.
    //
    // **That drain was latent, not live** — say it here, not three paragraphs
    // down, because "task 8b closed a real-disk leak" read alone is the wrong
    // impression. `path_is_ours` is engine-OR-client while the client's
    // `RootMap` is the engine's roots plus the staging alias, so "engine
    // accepts, client declines" cannot arise; `RootMap::decide` denies
    // `NotFound`/`Dir`/`Tombstone` before any tramp call; neither
    // `Decision::Redirect` arm calls `tag_under_root`, so a redirected handle
    // never enters `DIR_TABLE`; and a director-served directory is a
    // `fuse_synth` handle the drain could not drain. Reaching the branch in a
    // test took reverting gate 3 task 5 *as well* as forcing the predicate
    // disagreement. The value of removing it is that enumeration no longer
    // depends, silently and untested, on another gate's invariant.
    //
    // `drain_real`, `drain_real_classic` and `parse_full_dir_info` are deleted
    // with it, so containment here is structural rather than conditional:
    // no code remains that can read a real directory into a served listing.
    //
    // The two ways of reaching case 2 answer the same way and are counted
    // separately, because they are different failures:
    //
    // - **No client at all.** Standalone mode is retired (see
    //   `fuse_client::FuseInitError`): bootstrap aborts the launch when the
    //   ring cannot be attached, and `try_init_from_env` runs before the
    //   engine is built and before any detour installs, so an injected process
    //   always has a client by the time a hook can fire.
    // - **A client that does not recognise this directory.** The engine's root
    //   notion accepted the path at open time and the client's did not — which
    //   the superset argument above says cannot happen, but these two
    //   predicates *have* drifted apart before, for five spellings at once,
    //   and the comment on `path_is_ours` says plainly that they "can differ".
    //   Its own counter (`contained`) so a future drift is a number in the
    //   report rather than a directory that mysteriously lists nothing.
    //
    // **Nothing reaches either one today, including this crate's own tests.**
    // An earlier draft claimed `hook_enum_parity`/`hook_relative_paths` did,
    // since they install with no ring; they do not. Their `Data` is
    // overlay-backed, so `Engine::decide` answers `Redirect`, which never
    // tags the handle — those listings leave on the untracked branch above,
    // against the overlay's own physical path. Measured with a probe in each
    // branch, not argued: zero hits on both, in all three shim enumeration
    // tests. So this arm and `Engine::overlay_listing`'s only call site are
    // dead code. Keep both anyway: a branch that would otherwise fail *open*
    // is exactly the one worth having fail closed, and the day it comes back
    // to life is the day someone changes a predicate.
    //
    // One consequence worth stating, because a reviewer read the other way
    // round: this arm calls `overlay_listing` with an **empty base**, so
    // `Overlay::apply_to_listing`'s handling of a `merged` listing is
    // unreachable from production even if this arm revives with today's call
    // shape.
    //
    // **Gate 5, Task 7 changed what that costs.** It used to mean the only
    // implementation of marker-hiding sat behind two dead callers while the
    // live director branch below went without. The filtering now lives in
    // `overlay::strip_whiteout_markers`, which that branch calls directly and
    // `apply_to_listing` also calls — so the dead pair is kept for the
    // fail-closed reason above and no longer holds a second, divergent copy
    // of anything that matters. What is left dead in `apply_to_listing` is
    // its *physical* overlay-directory scan, which answers a case the live
    // branch does not have (a marker on disk that the incoming listing does
    // not carry).
    //
    // The ring round trip and the overlay's own `read_dir` both call out, so
    // the lock must NOT be held here (NtClose also takes it).
    let rebuilt = if need_build {
        let wildcard = wildcard_of(file_name);
        let routed = crate::fuse_client::global()
            .and_then(|c| c.vpath_under_root(&dir_path).map(|hit| (c, hit)));
        match routed {
            Some((client, (root, vpath))) => {
                let vp = if vpath.is_empty() { "." } else { vpath.as_str() };
                let items = match client.readdir(root, vp) {
                    Ok(entries) => {
                        let items: Vec<DirItem> = entries
                            .into_iter()
                            .map(|e| DirItem {
                                name: e.name,
                                is_dir: e.is_dir,
                                size: e.size,
                                mtime: e.mtime,
                            })
                            .collect();
                        // **Gate 5, Task 7 — the phantom whiteout marker,
                        // closed.** This branch used to hand the director's
                        // answer to the game verbatim, and the director's
                        // answer carries the shim's own markers: it mounts the
                        // shim overlay directory as its write layer
                        // (`overlay_layer_dir`) and spells whiteouts
                        // `.wh.<name>`, not `<name>.__vfs_wh__`, so ours come
                        // back as ordinary files. That showed the game a
                        // phantom `<file>.__vfs_wh__` entry *and* left the
                        // file it names listed.
                        //
                        // **Before the wildcard filter, not after** — see
                        // `strip_whiteout_markers`, which also records why the
                        // fix is here rather than in a shared spelling.
                        let mut items = crate::overlay::strip_whiteout_markers(items);
                        if let Some(ref w) = wildcard {
                            items.retain(|i| {
                                vfs_core::wildcard_match(w, &i.name)
                                    || i.name.eq_ignore_ascii_case(w)
                            });
                        }
                        items
                    }
                    Err(_) => Vec::new(),
                };
                // Two things this does **not** fix, both re-derived for this
                // task rather than inherited from the note that used to sit
                // here (which blamed a route gate 5 Task 4 had already
                // deleted):
                //
                // 1. **Enumeration only.** A marker still does not hide its
                //    target from an `open` through the director:
                //    `OverlayProvider::hidden_by_whiteout` looks for its own
                //    `.wh.` spelling, and there is no per-open hook here that
                //    could ask without a `stat` on every read.
                // 2. **New markers can still be minted under a live
                //    director.** `delete_hook` asks the client before
                //    `Engine::whiteout`, so a path-based delete routes; but
                //    `setinfo_hook`'s engine branch asks the engine *only*, so
                //    a handle-based delete on a non-synthetic under-root
                //    handle (inherited, pre-injection, or
                //    `allow_disk_fallthrough`) writes a shim-spelled marker
                //    into the director's own upper without the director ever
                //    hearing about the delete. That is a divergence between
                //    the two delete routes, not a listing defect, and it is
                //    recorded in gate 5's Task 7/8 report rather than changed
                //    at the end of a gate.
                Some((items, crate::hookstats::ReadDirSource::Director))
            }
            None => {
                // No real base to layer onto — that is the whole point. An
                // overlay-only listing is `overlay_listing` over an empty
                // base, which also means every entry now passes through the
                // wildcard filter: `apply_to_listing` only filters what it
                // *adds*, so the drained base used to skip the filter
                // entirely and answer `*.esp` with the whole directory.
                let items = match ENGINE.get() {
                    Some(engine) => engine.overlay_listing(&dir_path, &[], wildcard.as_deref()),
                    None => Vec::new(),
                };
                Some((items, crate::hookstats::ReadDirSource::ContainedNoDirector))
            }
        }
    } else {
        None
    };

    // Phase 3 (locked): store the built listing (if rebuilt) and serve a slice.
    //
    // **The caller's buffer is filled after the guard is released, never under
    // it.** `write_dir_info` writes into a scratch buffer we own; the copy into
    // `info` happens below, unlocked.
    //
    // That ordering is the fix for the intermittent hang traced on 2026-09-02.
    // `info` belongs to the caller and may lie inside one of our own
    // demand-paged regions, so touching it can fault into `lazy_section`, which
    // does file I/O, whose `NtClose` re-enters the shim and takes
    // `DIR_TABLE.lock()` again. `std::sync::Mutex` is not reentrant, so that
    // second acquisition blocked forever on a lock the same thread already
    // held: zero CPU, one thread, and immune to `TerminateProcess`. Measured
    // three times with `VFS_SHIM_BREADCRUMB` — `threads=1`, `entries - exits =
    // 2`, `mark=TABLES`.
    //
    // A scratch buffer rather than cloning the entries: the copy is bounded by
    // `length`, whereas a directory listing is unbounded.
    let mut scratch = vec![0u8; length as usize];
    let result = {
        let mut table = match DIR_TABLE.lock() {
            Ok(t) => t,
            Err(_) => return passthrough(),
        };
        let tracked = match table.get_mut(&key) {
            Some(t) => t,
            None => return passthrough(),
        };
        if let Some((entries, source)) = rebuilt {
            crate::hookstats::note_readdir(
                &dir_path,
                wildcard_of(file_name).as_deref(),
                entries.len(),
                source,
            );
            tracked.state = Some(EnumState { entries, cursor: 0 });
        }
        let st = match tracked.state.as_mut() {
            Some(s) => s,
            None => return passthrough(),
        };
        let result = write_dir_info(class, &st.entries[st.cursor..], &mut scratch, single);
        st.cursor += result.count;
        result
    };

    // Unlocked from here: a fault on `info` can now re-enter the shim freely.
    if result.bytes > 0 {
        let buf = core::slice::from_raw_parts_mut(info as *mut u8, length as usize);
        buf[..result.bytes].copy_from_slice(&scratch[..result.bytes]);
    }

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
    /// `spoofed_object_name` adopts the host's prefix rather than assuming one.
    /// Both forms are measured facts (2026-09-01): Windows answers
    /// `\Device\HarddiskVolumeN\...`, Wine answers `\??\C:\...`. A hook that
    /// emitted one fixed form would be wrong on one of the two hosts.
    #[test]
    fn spoofed_object_name_adopts_the_hosts_prefix() {
        // Wine's form: the DOS portion of the virtual path, re-prefixed. The
        // device lookup must not even be consulted here -- it disagrees with
        // Wine's own answer, so consulting it would be actively misleading.
        assert_eq!(
            spoofed_object_name(
                r"\??\C:\backing\blob.dat",
                r"\??\C:\root\mod.esp",
                |_| panic!("QueryDosDeviceW must not be consulted on the \\??\\ branch"),
            ),
            Some(r"\??\C:\root\mod.esp".to_string())
        );
        // Windows' form: the VIRTUAL path's drive resolved to a device, with the
        // virtual path's volume-relative remainder appended. Note the device
        // comes from the virtual drive, not from the real answer's device --
        // the backing file may live on another volume entirely.
        assert_eq!(
            spoofed_object_name(
                r"\Device\HarddiskVolume7\backing\blob.dat",
                r"\??\C:\root\mod.esp",
                |d| {
                    assert_eq!(d, "C:");
                    Some(r"\Device\HarddiskVolume3".to_string())
                },
            ),
            Some(r"\Device\HarddiskVolume3\root\mod.esp".to_string())
        );
    }

    /// Everything this function cannot build honestly must come back `None`, so
    /// the hook passes the host's own answer through. A wrong name is worse
    /// than the backing one: it is undiagnosable.
    #[test]
    fn spoofed_object_name_declines_rather_than_guesses() {
        let dev = |_: &str| Some(r"\Device\HarddiskVolume3".to_string());
        // A convention we do not recognise. `\Device\Mup\...`-style names reach
        // the `\Device\` branch legitimately, but a bare NT object path such as
        // a named pipe or a mailslot root does not.
        assert_eq!(
            spoofed_object_name(r"\BaseNamedObjects\SomeMutex", r"\??\C:\root\mod.esp", dev),
            None
        );
        assert_eq!(spoofed_object_name("", r"\??\C:\root\mod.esp", dev), None);
        // A virtual path with no drive letter to resolve.
        assert_eq!(
            spoofed_object_name(r"\Device\HarddiskVolume3\x", r"\??\UNC\server\share\f", dev),
            None
        );
        assert_eq!(
            spoofed_object_name(r"\Device\HarddiskVolume3\x", r"\??\", dev),
            None
        );
        // `C:relative` is not a rooted path; appending it to a device prefix
        // would splice two names together (`\Device\HarddiskVolume3relative`).
        assert_eq!(
            spoofed_object_name(r"\Device\HarddiskVolume3\x", r"\??\C:relative", dev),
            None
        );
        // The device lookup failing is a decline, not a fallback: with no
        // device name there is nothing to build the Windows form out of.
        assert_eq!(
            spoofed_object_name(r"\Device\HarddiskVolume3\x", r"\??\C:\root\mod.esp", |_| None),
            None
        );
    }

    /// A drive root has an empty remainder, and both prefixes must survive it
    /// rather than producing a trailing-separator variant of the volume name.
    #[test]
    fn spoofed_object_name_handles_a_drive_root() {
        assert_eq!(
            spoofed_object_name(r"\??\C:\x", r"\??\C:", |_| unreachable!()),
            Some(r"\??\C:".to_string())
        );
        assert_eq!(
            spoofed_object_name(r"\Device\HarddiskVolume3\x", r"\??\C:", |_| Some(
                r"\Device\HarddiskVolume3".to_string()
            )),
            Some(r"\Device\HarddiskVolume3".to_string())
        );
    }

    /// `PATH_TABLE` holds `\??\`-prefixed paths today, but `record_path` stores
    /// whatever the open was decoded as. The `\\?\` long-path prefix and a bare
    /// Win32 path must both be understood, or a handle opened by one of those
    /// spellings silently declines the spoof and leaks.
    #[test]
    fn spoofed_object_name_accepts_every_prefix_path_table_can_hold() {
        for vpath in [r"\??\C:\root\mod.esp", r"\\?\C:\root\mod.esp", r"C:\root\mod.esp"] {
            assert_eq!(
                spoofed_object_name(r"\??\C:\backing", vpath, |_| unreachable!()),
                Some(r"\??\C:\root\mod.esp".to_string()),
                "vpath = {vpath}"
            );
        }
    }

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
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const FILE_APPEND_DATA: u32 = 0x0004;
        assert!(!is_append_only(GENERIC_WRITE | FILE_APPEND_DATA));
    }

    /// The genuine append-only shape — `FILE_APPEND_DATA` with neither
    /// `FILE_WRITE_DATA` nor `GENERIC_WRITE` — must still be classified as
    /// append-only. `Rust`'s `OpenOptions::append(true)` (without
    /// `.write(true)`) requests exactly this.
    #[test]
    fn append_data_alone_is_append_only() {
        const FILE_APPEND_DATA: u32 = 0x0004;
        assert!(is_append_only(FILE_APPEND_DATA));
    }

    /// `FILE_WRITE_DATA` set explicitly alongside `FILE_APPEND_DATA` is full
    /// write access, not append-only — unchanged by the fix, kept here so a
    /// future edit cannot silently invert it.
    #[test]
    fn explicit_write_data_with_append_data_is_not_append_only() {
        const FILE_WRITE_DATA: u32 = 0x0002;
        const FILE_APPEND_DATA: u32 = 0x0004;
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
        assert!(dir_open_downgrades(1), "FILE_OPEN opens an existing directory");
        assert!(dir_open_downgrades(3), "FILE_OPEN_IF opens an existing directory");
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

    // ---------------------------------------------------------------------
    // Panic containment at the `extern "system"` boundary.
    //
    // These drive test-only bodies registered through the *same*
    // `hook_entry_points!` macro every real detour uses, so what they exercise
    // is the generated wrapper and `contain_panic`, not a hand-written
    // stand-in. `no_extern_hook_bypasses_the_panic_containment_macro` is the
    // other half: it pins that no real hook can be defined any other way.
    //
    // A real hook cannot be made to panic from a unit test — every one of them
    // reaches its interesting code only through a live trampoline into ntdll,
    // and a trampoline is itself `extern "system"`, so a panic planted there
    // would abort at *its* boundary before ever reaching ours.
    // ---------------------------------------------------------------------

    use std::thread::LocalKey;

    thread_local! {
        /// Set by a frame standing in for the game code below the detour.
        static CALLER_FRAME_DROPPED: Cell<bool> = const { Cell::new(false) };
        /// Set by a frame *inside* the hook body, above the catch.
        static HOOK_FRAME_DROPPED: Cell<bool> = const { Cell::new(false) };
    }

    struct DropFlag(&'static LocalKey<Cell<bool>>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.with(|c| c.set(true));
        }
    }

    unsafe fn panicking_hook_body(reached: *mut u32) -> NTSTATUS {
        let _frame = DropFlag(&HOOK_FRAME_DROPPED);
        if !reached.is_null() {
            *reached = 1;
        }
        panic!("deliberate test panic inside a hook body");
    }

    unsafe fn counted_panicking_hook_body() -> NTSTATUS {
        panic!("deliberate test panic, counted by name");
    }

    /// Panics with the thread's reentrancy guard held, which is the case
    /// requirement 2 is about: [`ShimIoGuard`] is the only thing that lowers
    /// `HOOK_REENTER` again.
    unsafe fn guarded_panicking_hook_body() -> NTSTATUS {
        let Some(_io) = ShimIoGuard::enter() else {
            return STATUS_ACCESS_DENIED;
        };
        panic!("deliberate test panic with shim I/O in flight");
    }

    /// Reports what a *real* hook's first branch would see: `in_hook_reenter`
    /// decides between serving the call and handing it to real ntdll.
    unsafe fn reentrancy_probe_body() -> NTSTATUS {
        if in_hook_reenter() {
            STATUS_ACCESS_DENIED
        } else {
            STATUS_SUCCESS
        }
    }

    unsafe fn panicking_bool_hook_body() -> i32 {
        panic!("deliberate test panic in a BOOL-returning hook");
    }

    hook_entry_points! {
        fn panicking_hook = panicking_hook_body(reached: *mut u32) -> NTSTATUS
            as "NtTestPanic", on_panic STATUS_HOOK_PANICKED;

        fn counted_panicking_hook = counted_panicking_hook_body() -> NTSTATUS
            as "NtTestCounted", on_panic STATUS_HOOK_PANICKED;

        fn guarded_panicking_hook = guarded_panicking_hook_body() -> NTSTATUS
            as "NtTestPanicUnderShimIo", on_panic STATUS_HOOK_PANICKED;

        fn reentrancy_probe = reentrancy_probe_body() -> NTSTATUS
            as "NtTestReentrancyProbe", on_panic STATUS_HOOK_PANICKED;

        fn panicking_bool_hook = panicking_bool_hook_body() -> i32
            as "TestPanicBool", on_panic {
            unsafe { windows_sys::Win32::Foundation::SetLastError(ERROR_INTERNAL_ERROR) };
            0
        };
    }

    /// The caller gets a status back at all — and it is a *failure* status.
    /// Both halves matter: without the first the process is gone, and without
    /// the second the game reads an output buffer the panicking body never
    /// filled in.
    #[test]
    fn a_panicking_hook_returns_a_failure_status_instead_of_aborting() {
        let mut reached = 0u32;
        let st = unsafe { panicking_hook(&mut reached) };
        assert_eq!(reached, 1, "the body must have run far enough to panic");
        assert_eq!(st, STATUS_UNSUCCESSFUL);
        assert!(st < 0, "NTSTATUS {st:#x} has no severity bits — a caller reads it as success");
    }

    /// The unwind stops at the `extern "system"` boundary and does not run the
    /// caller's destructors on its way out.
    ///
    /// The caller's frame here stands in for the game's. Measured 2026-08-16,
    /// an uncontained panic did not unwind into it either — rustc's forced
    /// `panic_cannot_unwind` sits at *our* boundary, so the game's frames were
    /// never at risk and the process simply died instead. What this pins is
    /// that adding the catch did not move the boundary outwards: the frame
    /// below the hook is untouched when the hook returns, and drops normally
    /// afterwards like any other.
    ///
    /// The second assertion is the deliberate other side of it. The hook's own
    /// frame **is** unwound, and must be: that is the mechanism releasing
    /// `ShimIoGuard`, and a containment that skipped it would trade a crash for
    /// a thread that silently stops being virtualized.
    #[test]
    fn a_panicking_hook_does_not_run_its_callers_destructors() {
        CALLER_FRAME_DROPPED.with(|c| c.set(false));
        HOOK_FRAME_DROPPED.with(|c| c.set(false));
        {
            let _caller = DropFlag(&CALLER_FRAME_DROPPED);
            let mut reached = 0u32;
            let st = unsafe { panicking_hook(&mut reached) };
            assert_eq!(st, STATUS_UNSUCCESSFUL);
            assert!(
                !CALLER_FRAME_DROPPED.with(Cell::get),
                "the unwind escaped the hook and ran a caller frame's Drop"
            );
            assert!(
                HOOK_FRAME_DROPPED.with(Cell::get),
                "the hook body's own frame was not unwound — nothing would release ShimIoGuard"
            );
        }
        assert!(
            CALLER_FRAME_DROPPED.with(Cell::get),
            "the caller frame must still drop normally once it goes out of scope"
        );
    }

    /// After a caught panic the thread is still usable. A panic taken while the
    /// reentrancy guard was held used to be unrecoverable in the worst possible
    /// way — `HOOK_REENTER` stuck at 1 means every later hook call on that
    /// thread takes the fast path to real ntdll, so the process quietly stops
    /// being virtualized while every counter keeps reporting ordinary activity.
    #[test]
    fn a_caught_panic_leaves_no_reentrancy_state_held() {
        assert!(!in_hook_reenter(), "test thread started inside the guard");
        assert_eq!(unsafe { reentrancy_probe() }, STATUS_SUCCESS);

        assert_eq!(unsafe { guarded_panicking_hook() }, STATUS_UNSUCCESSFUL);

        assert!(!in_hook_reenter(), "HOOK_REENTER stayed raised after the panic");
        assert_eq!(
            unsafe { reentrancy_probe() },
            STATUS_SUCCESS,
            "the next call on this thread took the reentrant fast path to real ntdll"
        );
        assert!(
            ShimIoGuard::enter().is_some(),
            "no further shim-initiated I/O could be started on this thread"
        );
    }

    /// A caught panic is a bug that just happened; it has to be countable.
    #[test]
    fn every_caught_panic_is_counted_under_its_own_entry_point() {
        let before_total = crate::hookstats::hook_panics_total();
        assert_eq!(crate::hookstats::hook_panic_count("NtTestCounted"), 0);
        assert_eq!(unsafe { counted_panicking_hook() }, STATUS_UNSUCCESSFUL);
        assert_eq!(
            crate::hookstats::hook_panic_count("NtTestCounted"),
            1,
            "the panic was not attributed to the entry point it happened in"
        );
        // Other panic tests may run concurrently, so the total is a lower bound.
        assert!(crate::hookstats::hook_panics_total() > before_total);
    }

    /// `CreateProcessInternalW` returns a Win32 `BOOL`, and the uniform
    /// `STATUS_UNSUCCESSFUL` is non-zero in that ABI — i.e. `TRUE`. A caller
    /// told the process was created goes on to use a `PROCESS_INFORMATION`
    /// nothing filled in.
    #[test]
    fn a_panicking_bool_hook_reports_false_rather_than_a_status() {
        assert_ne!(STATUS_HOOK_PANICKED, 0, "the trap this test exists for is gone");
        let r = unsafe { panicking_bool_hook() };
        assert_eq!(r, 0, "a BOOL-returning hook must fail with FALSE");
        assert_eq!(
            unsafe { windows_sys::Win32::Foundation::GetLastError() },
            ERROR_INTERNAL_ERROR,
            "a failing BOOL Win32 function must set the last error, not leave a stale one"
        );
    }

    /// Structural: **every** `extern "system"` function in the injected DLL must
    /// contain its own panic, by calling [`contain_panic`] in its body.
    ///
    /// Requirement 1 of task 1 is that *every* entry point contains its panic,
    /// and the behaviour tests above can only ever demonstrate that for the hooks
    /// they drive. This is what makes the claim total, and what stops hook number
    /// twenty-two from being written the old way — which would compile, install,
    /// and abort the game exactly as before, with nothing in any test to say so.
    ///
    /// ## Its first version had a hole, and the hole was real
    ///
    /// The check used to be `include_str!("hook.rs")` and an assertion that the
    /// only definition found was the macro's `$wrapper`. That is a *hand-written
    /// file list of one*, and it missed `lazy_section.rs`'s `veh_handler` — an
    /// uncontained `extern "system"` entry point which additionally carried a raw
    /// `IN_VEH.set(true)` / `set(false)` pair, the exact latch defect task 1
    /// removed everywhere else. It went unseen through the whole of stage 4
    /// *because the guard was believed*.
    ///
    /// So the enumeration is derived, not written:
    ///
    ///  * the **directories** are the two crates that compose the injected DLL —
    ///    this one and `vfs-shim-dll`. `vfs-payload` is deliberately absent: it is
    ///    `#![no_std]` with `panic = "abort"` and its own workspace, so it has no
    ///    unwind to contain and could not call this function if it wanted to;
    ///  * the **files** are read off those directories recursively at test time,
    ///    so a new module cannot be added outside the check;
    ///  * the **rule** is per-definition rather than a name list, so a new entry
    ///    point is checked the moment it is written and no allowlist has to be
    ///    edited to admit a legitimate one.
    #[test]
    fn no_extern_hook_bypasses_the_panic_containment_macro() {
        const MARKER: &str = "extern \"system\" fn ";
        // Spelled with `concat!` so the needle does not appear verbatim in the
        // files it is searching (this file is one of them).
        let containment = concat!("contain", "_panic");

        let this_crate = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let dirs = [
            this_crate.join("src"),
            this_crate
                .parent()
                .expect("crates/")
                .join("vfs-shim-dll")
                .join("src"),
        ];

        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let entries = std::fs::read_dir(dir)
                .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut files: Vec<std::path::PathBuf> = Vec::new();
        for d in &dirs {
            assert!(d.is_dir(), "{} is not a directory", d.display());
            walk(d, &mut files);
        }
        files.sort();
        // A broken enumeration must fail loudly rather than pass vacuously.
        assert!(
            files.iter().any(|p| p.ends_with("hook.rs"))
                && files.iter().any(|p| p.ends_with("lazy_section.rs"))
                && files.len() >= 14,
            "the enumeration must have found both crates' sources — got {files:?}"
        );

        /// From the byte after `fn NAME`, the text of the function's body,
        /// found by matching braces from the first `{`. Naive about braces in
        /// strings and comments, which is sound in the conservative direction:
        /// a mismatch makes the body *longer*, never shorter, and the check
        /// only ever asks whether a needle is present.
        fn body_after(src: &str, from: usize) -> &str {
            let Some(open) = src[from..].find('{').map(|i| from + i) else {
                return "";
            };
            let bytes = src.as_bytes();
            let mut depth = 0usize;
            for i in open..bytes.len() {
                match bytes[i] {
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 {
                            return &src[open..=i];
                        }
                    }
                    _ => {}
                }
            }
            &src[open..]
        }

        let mut checked = 0usize;
        for path in &files {
            let src = std::fs::read_to_string(path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let rel = path
                .strip_prefix(this_crate.parent().expect("crates/"))
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            for (i, _) in src.match_indices(MARKER) {
                let rest = &src[i + MARKER.len()..];
                let end = rest
                    .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                    .unwrap_or(rest.len());
                // A zero-length name is prose or a bare fn-pointer type, not a
                // definition (`fn(` in a type alias has no space before the
                // paren).
                if end == 0 {
                    continue;
                }
                let name = &rest[..end];
                let body = body_after(rest, end);
                checked += 1;
                assert!(
                    body.contains(containment),
                    "{rel}: `extern \"system\" fn {name}` does not call `{containment}` in \
                     its body, so a panic in it unwinds out of an `extern` frame and \
                     aborts the game process (0xC0000409) instead of returning a failure. \
                     Wrap the body: `contain_panic(\"{name}\", || …, || <failure value>)`, \
                     the same containment all twenty ntdll detours use. If it is an ntdll \
                     detour, add it to `hook_entry_points!` and get the wrapper for free."
                );
            }
        }
        // 20 detours + 2 test hooks in this file's `hook_entry_points!` are all
        // one generated `$wrapper`, so the count is small on purpose: the macro,
        // `veh_handler`, `DllMain`, `vfs_shim_sync_bootstrap`.
        assert!(
            checked >= 4,
            "only {checked} `extern \"system\"` definitions were examined; the scan is \
             not finding them"
        );

        // And nothing may register a raw body as a detour: the bodies are not
        // `extern "system"`, so installing one is both an ABI error and a way
        // around the containment.
        assert!(
            !include_str!("hook.rs").contains(concat!("_hook_body", " as *const ()")),
            "a hook body was installed as a detour, bypassing its wrapper"
        );
    }
}
