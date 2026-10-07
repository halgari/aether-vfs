//! The ntdll detours. ALL `unsafe` in the crate lives here.
#![allow(unsafe_code)]

mod close;
mod entry;
mod file_open;
mod handles;
mod install;
mod path;
#[cfg(test)]
mod test_support;
// @mods

use self::close::*;
pub(crate) use self::entry::ShimIoGuard;
pub use self::entry::as_shim_io_for_tests;
pub use self::entry::contain_panic;
use self::entry::*;
use self::file_open::*;
use self::handles::*;
pub use self::install::HookGuard;
pub use self::install::InstallError;
pub use self::install::install;
pub use self::install::install_late;
pub use self::install::registry_detours_installed;
pub use self::install::skipped_detours;
use self::install::*;
use self::path::*;
#[cfg(test)]
use self::test_support::*;
// @uses

use core::ffi::c_void;
use std::sync::OnceLock;

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

use vfs_redirect::{
    DirInfoClass, DirItem, DirStatus, SYNTH_FILETIME, write_dir_info, write_file_name_info,
};
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW,
};

use crate::engine::Engine;
use crate::inject::{inject_child, re_suspend};
use crate::ntdef::{
    FILE_ALL_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_TAG_INFORMATION, FILE_BASIC_INFORMATION, FILE_DEVICE_DISK,
    FILE_DISPOSITION_DELETE, FILE_DISPOSITION_INFORMATION, FILE_DISPOSITION_INFORMATION_EX,
    FILE_END_OF_FILE_INFORMATION, FILE_FS_DEVICE_INFORMATION, FILE_ID_INFORMATION,
    FILE_INTERNAL_INFORMATION, FILE_NAME_INFORMATION, FILE_NETWORK_OPEN_INFORMATION,
    FILE_NORMALIZED_NAME_INFORMATION, FILE_POSITION_INFORMATION, FILE_RENAME_INFORMATION,
    FILE_RENAME_INFORMATION_EX, FILE_STANDARD_INFORMATION, FILE_STAT_INFORMATION,
    FileBasicInformation, FileEndOfFileInformation, FileFsDeviceInformation,
    FileInternalInformation, FileNetworkOpenInformation, FilePositionInformation,
    FileStandardInformation, NtCreateSectionFn, NtDeleteFileFn, OBJECT_NAME_INFORMATION,
    OBJECT_NAME_INFORMATION_HEADER, ObjectAttributes, SEC_IMAGE, SL_RESTART_SCAN,
    SL_RETURN_SINGLE_ENTRY, STATUS_ACCESS_DENIED, STATUS_BUFFER_OVERFLOW, STATUS_END_OF_FILE,
    STATUS_FILE_IS_A_DIRECTORY, STATUS_INFO_LENGTH_MISMATCH, STATUS_INVALID_FILE_FOR_SECTION,
    STATUS_INVALID_HANDLE, STATUS_NO_MORE_FILES, STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_INVALID,
    STATUS_OBJECT_NAME_NOT_FOUND, STATUS_OBJECT_PATH_NOT_FOUND, STATUS_SECTION_TOO_BIG,
    STATUS_SUCCESS, STATUS_UNSUCCESSFUL, UnicodeString,
};
use crate::overlay::OverlayState;

static ENGINE: OnceLock<Engine> = OnceLock::new();
/// The volume every synthetic handle says it is on, where a volume serial
/// number is asked for together with a file id: two ids are only comparable
/// on one volume, and every virtual file is on this one.
///
/// Fits 32 bits, because the same number is what `FileFsVolumeInformation`
/// reports — and so what `GetFileInformationByHandle` puts in
/// `dwVolumeSerialNumber` — and the two must not disagree about which volume
/// one handle is on.
const SYNTH_VOLUME_SERIAL: u64 = 0x5646_5300;

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

/// The unhooked registry entry points, for `regkeys`.
pub(crate) unsafe fn reg_real() -> crate::regkeys::Real {
    crate::regkeys::Real {
        open_ex: TRAMP_OPEN_KEY_EX.get(),
        query: TRAMP_QUERY_KEY.get(),
        close: TRAMP_CLOSE.get(),
        dup: TRAMP_DUP.get(),
        enum_key: TRAMP_ENUM_KEY.get(),
        query_value: TRAMP_QUERY_VALUE.get(),
        enum_value: TRAMP_ENUM_VALUE.get(),
        query_multiple: TRAMP_QUERY_MULTIPLE.get(),
        query_object: TRAMP_QOBJ.get(),
    }
}

/// Whether a registry hook should go straight to its trampoline: the overlay is off (ruling:
/// nothing but this check), or this thread is inside the shim's own work.
fn reg_bypass() -> bool {
    !crate::regclient::enabled() || in_hook_reenter()
}

/// `NtOpenKey` hook. See `regkeys::open_or_create`.
unsafe fn open_key_hook_body(
    key: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::OpenKey);
    let Some(tramp) = TRAMP_OPEN_KEY.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if reg_bypass() {
        return tramp(key, access, oa);
    }
    // Held for the whole call: a registry or file call this thread makes while the hook works
    // (the shim's own) goes straight to ntdll.
    let Some(_io) = ShimIoGuard::enter() else {
        return tramp(key, access, oa);
    };
    crate::regkeys::open_or_create(
        &reg_real(),
        key,
        access,
        oa,
        crate::regkeys::Call::Open,
        &mut |oa| tramp(key, access, oa),
    )
    .status
}

/// `NtOpenKeyEx` hook. See `regkeys::open_or_create`.
unsafe fn open_key_ex_hook_body(
    key: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    options: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::OpenKeyEx);
    let Some(tramp) = TRAMP_OPEN_KEY_EX.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if reg_bypass() {
        return tramp(key, access, oa, options);
    }
    let Some(_io) = ShimIoGuard::enter() else {
        return tramp(key, access, oa, options);
    };
    crate::regkeys::open_or_create(
        &reg_real(),
        key,
        access,
        oa,
        crate::regkeys::Call::Open,
        &mut |oa| tramp(key, access, oa, options),
    )
    .status
}

/// `NtCreateKey` hook. With the overlay on, the real `NtCreateKey` is never called: a key that
/// exists for real is opened (`NtOpenKeyEx` trampoline, the caller's access and open options)
/// and reported as `REG_OPENED_EXISTING_KEY`; one that does not is created in the overlay.
/// `TitleIndex` and `Class` are not modelled by the overlay and are ignored for its keys.
unsafe fn create_key_hook_body(
    key: *mut HANDLE,
    access: u32,
    oa: *const ObjectAttributes,
    title_index: u32,
    class: *const UnicodeString,
    options: u32,
    disposition: *mut u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::CreateKey);
    let Some(tramp) = TRAMP_CREATE_KEY.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if !crate::regclient::enabled() {
        return tramp(key, access, oa, title_index, class, options, disposition);
    }
    // With the overlay on the real create is never made, not even for the shim's own work: a
    // create that cannot be examined here is refused.
    if in_hook_reenter() {
        return STATUS_UNSUCCESSFUL;
    }
    let Some(_io) = ShimIoGuard::enter() else {
        return STATUS_UNSUCCESSFUL;
    };
    let Some(open_ex) = TRAMP_OPEN_KEY_EX.get() else {
        // No way to open an existing key without the real create: refuse rather than write.
        return STATUS_UNSUCCESSFUL;
    };
    let open_options = crate::regkeys::open_options_of_create(options);
    let out = crate::regkeys::open_or_create(
        &reg_real(),
        key,
        access,
        oa,
        crate::regkeys::Call::Create { options },
        &mut |oa| open_ex(key, access, oa, open_options),
    );
    if out.status >= 0 && !disposition.is_null() {
        *disposition = out.disposition;
    }
    out.status
}

/// `NtDuplicateObject` hook: duplicates of tracked key handles stay tracked. See
/// `regkeys::duplicate`.
unsafe fn dup_hook_body(
    src_process: HANDLE,
    src: HANDLE,
    dst_process: HANDLE,
    dst: *mut HANDLE,
    access: u32,
    attributes: u32,
    options: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::DuplicateObject);
    let Some(tramp) = TRAMP_DUP.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if reg_bypass() {
        return tramp(
            src_process,
            src,
            dst_process,
            dst,
            access,
            attributes,
            options,
        );
    }
    match crate::regkeys::duplicate(
        &reg_real(),
        src_process,
        src,
        dst_process,
        dst,
        access,
        attributes,
        options,
    ) {
        Some(st) => st,
        None => tramp(
            src_process,
            src,
            dst_process,
            dst,
            access,
            attributes,
            options,
        ),
    }
}

/// `NtQueryKey` hook. See `regquery::query_key`.
unsafe fn query_key_hook_body(
    key: HANDLE,
    class: u32,
    info: *mut c_void,
    length: u32,
    ret_len: *mut u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QueryKey);
    let Some(tramp) = TRAMP_QUERY_KEY.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if reg_bypass() {
        return tramp(key, class, info, length, ret_len);
    }
    let Some(_io) = ShimIoGuard::enter() else {
        return tramp(key, class, info, length, ret_len);
    };
    crate::regquery::query_key(&reg_real(), key as isize, class, info, length, ret_len)
}

/// `NtEnumerateKey` hook. See `regquery::enumerate_key`.
unsafe fn enum_key_hook_body(
    key: HANDLE,
    index: u32,
    class: u32,
    info: *mut c_void,
    length: u32,
    ret_len: *mut u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::EnumerateKey);
    let Some(tramp) = TRAMP_ENUM_KEY.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if reg_bypass() {
        return tramp(key, index, class, info, length, ret_len);
    }
    let Some(_io) = ShimIoGuard::enter() else {
        return tramp(key, index, class, info, length, ret_len);
    };
    crate::regquery::enumerate_key(
        &reg_real(),
        key as isize,
        index,
        class,
        info,
        length,
        ret_len,
    )
}

/// `NtQueryValueKey` hook. See `regquery::query_value_key`.
unsafe fn query_value_hook_body(
    key: HANDLE,
    name: *const UnicodeString,
    class: u32,
    info: *mut c_void,
    length: u32,
    ret_len: *mut u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QueryValueKey);
    let Some(tramp) = TRAMP_QUERY_VALUE.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if reg_bypass() {
        return tramp(key, name, class, info, length, ret_len);
    }
    let Some(_io) = ShimIoGuard::enter() else {
        return tramp(key, name, class, info, length, ret_len);
    };
    crate::regquery::query_value_key(
        &reg_real(),
        key as isize,
        name,
        class,
        info,
        length,
        ret_len,
    )
}

/// `NtEnumerateValueKey` hook. See `regquery::enumerate_value_key`.
unsafe fn enum_value_hook_body(
    key: HANDLE,
    index: u32,
    class: u32,
    info: *mut c_void,
    length: u32,
    ret_len: *mut u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::EnumerateValueKey);
    let Some(tramp) = TRAMP_ENUM_VALUE.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if reg_bypass() {
        return tramp(key, index, class, info, length, ret_len);
    }
    let Some(_io) = ShimIoGuard::enter() else {
        return tramp(key, index, class, info, length, ret_len);
    };
    crate::regquery::enumerate_value_key(
        &reg_real(),
        key as isize,
        index,
        class,
        info,
        length,
        ret_len,
    )
}

/// `NtQueryMultipleValueKey` hook. See `regquery::query_multiple_value_key`.
unsafe fn query_multiple_hook_body(
    key: HANDLE,
    entries: *mut c_void,
    count: u32,
    buffer: *mut c_void,
    buffer_len: *mut u32,
    required: *mut u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QueryMultipleValueKey);
    let Some(tramp) = TRAMP_QUERY_MULTIPLE.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if reg_bypass() {
        return tramp(key, entries, count, buffer, buffer_len, required);
    }
    let Some(_io) = ShimIoGuard::enter() else {
        return tramp(key, entries, count, buffer, buffer_len, required);
    };
    crate::regquery::query_multiple_value_key(
        &reg_real(),
        key as isize,
        entries,
        count,
        buffer,
        buffer_len,
        required,
    )
}

/// Whether a registry *write* hook may go on with the overlay on: not when this thread is inside
/// the shim's own work (ruling: such a write is refused, never made for real). `Some(guard)` to
/// hold for the call.
fn reg_write_guard() -> Option<ShimIoGuard> {
    if in_hook_reenter() {
        return None;
    }
    ShimIoGuard::enter()
}

/// `NtSetValueKey` hook. With the overlay on, a write on a virtualised key goes to the director
/// (`regwrite::set_value_key`) and never to the real key; `TitleIndex` is ignored, as Windows
/// ignores it.
unsafe fn set_value_key_hook_body(
    key: HANDLE,
    name: *const UnicodeString,
    title_index: u32,
    ty: u32,
    data: *const c_void,
    size: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::SetValueKey);
    let Some(tramp) = TRAMP_SET_VALUE.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if !crate::regclient::enabled() {
        return tramp(key, name, title_index, ty, data, size);
    }
    let Some(_io) = reg_write_guard() else {
        return STATUS_UNSUCCESSFUL;
    };
    let _ws = crate::regclient::WriteScope::enter();
    match crate::regwrite::set_value_key(&reg_real(), key as isize, name, ty, data, size) {
        crate::regwrite::Write::Done(st) => st,
        crate::regwrite::Write::Pass => tramp(key, name, title_index, ty, data, size),
    }
}

/// `NtDeleteValueKey` hook. See `regwrite::delete_value_key`.
unsafe fn delete_value_key_hook_body(key: HANDLE, name: *const UnicodeString) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::DeleteValueKey);
    let Some(tramp) = TRAMP_DELETE_VALUE.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if !crate::regclient::enabled() {
        return tramp(key, name);
    }
    let Some(_io) = reg_write_guard() else {
        return STATUS_UNSUCCESSFUL;
    };
    let _ws = crate::regclient::WriteScope::enter();
    match crate::regwrite::delete_value_key(&reg_real(), key as isize, name) {
        crate::regwrite::Write::Done(st) => st,
        crate::regwrite::Write::Pass => tramp(key, name),
    }
}

/// `NtDeleteKey` hook. See `regwrite::delete_key`.
unsafe fn delete_key_hook_body(key: HANDLE) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::DeleteKey);
    let Some(tramp) = TRAMP_DELETE_KEY.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if !crate::regclient::enabled() {
        return tramp(key);
    }
    let Some(_io) = reg_write_guard() else {
        return STATUS_UNSUCCESSFUL;
    };
    let _ws = crate::regclient::WriteScope::enter();
    match crate::regwrite::delete_key(&reg_real(), key as isize) {
        crate::regwrite::Write::Done(st) => st,
        crate::regwrite::Write::Pass => tramp(key),
    }
}

/// `NtRenameKey` hook. See `regwrite::rename_key`.
unsafe fn rename_key_hook_body(key: HANDLE, new_name: *const UnicodeString) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::RenameKey);
    let Some(tramp) = TRAMP_RENAME_KEY.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if !crate::regclient::enabled() {
        return tramp(key, new_name);
    }
    let Some(_io) = reg_write_guard() else {
        return STATUS_UNSUCCESSFUL;
    };
    let _ws = crate::regclient::WriteScope::enter();
    match crate::regwrite::rename_key(&reg_real(), key as isize, new_name) {
        crate::regwrite::Write::Done(st) => st,
        crate::regwrite::Write::Pass => tramp(key, new_name),
    }
}

/// `NtSetInformationKey` hook. See `regwrite::set_information_key`.
unsafe fn set_info_key_hook_body(
    key: HANDLE,
    class: u32,
    info: *const c_void,
    length: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::SetInformationKey);
    let Some(tramp) = TRAMP_SET_INFO_KEY.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if !crate::regclient::enabled() {
        return tramp(key, class, info, length);
    }
    let Some(_io) = reg_write_guard() else {
        return STATUS_UNSUCCESSFUL;
    };
    let _ws = crate::regclient::WriteScope::enter();
    match crate::regwrite::set_information_key(&reg_real(), key as isize, class, info, length) {
        crate::regwrite::Write::Done(st) => st,
        crate::regwrite::Write::Pass => tramp(key, class, info, length),
    }
}

/// `NtFlushKey` hook. See `regwrite::flush_key`. A flush writes nothing the caller did not
/// already write, so the shim's own (re-entrant) calls pass through like the read hooks'.
unsafe fn flush_key_hook_body(key: HANDLE) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::FlushKey);
    let Some(tramp) = TRAMP_FLUSH_KEY.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if reg_bypass() {
        return tramp(key);
    }
    let Some(_io) = ShimIoGuard::enter() else {
        return tramp(key);
    };
    let _ws = crate::regclient::WriteScope::enter();
    match crate::regwrite::flush_key(&reg_real(), key as isize) {
        crate::regwrite::Write::Done(st) => st,
        crate::regwrite::Write::Pass => tramp(key),
    }
}

/// `NtNotifyChangeKey` hook. A key the overlay serves gets an overlay waiter
/// (`regnotify::notify`); anything else the real call.
#[allow(clippy::too_many_arguments)]
unsafe fn notify_key_hook_body(
    key: HANDLE,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    filter: u32,
    subtree: u8,
    buffer: *mut c_void,
    buffer_len: u32,
    asynchronous: u8,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::NotifyChangeKey);
    let Some(tramp) = TRAMP_NOTIFY_KEY.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    let pass = || {
        tramp(
            key,
            event,
            apc,
            apc_ctx,
            iosb,
            filter,
            subtree,
            buffer,
            buffer_len,
            asynchronous,
        )
    };
    if reg_bypass() {
        return pass();
    }
    let Some(_io) = ShimIoGuard::enter() else {
        return pass();
    };
    let args = crate::regnotify::Args {
        event,
        apc,
        apc_ctx,
        iosb,
        subtree: subtree != 0,
        asynchronous: asynchronous != 0,
        count: 0,
    };
    match crate::regnotify::notify(&reg_real(), key as isize, &args) {
        crate::regnotify::Notify::Done(st) => st,
        crate::regnotify::Notify::Pass => pass(),
    }
}

/// `NtNotifyChangeMultipleKeys` hook: as `NtNotifyChangeKey` for the master key; subordinate
/// keys on a key the overlay serves are `STATUS_NOT_SUPPORTED`.
#[allow(clippy::too_many_arguments)]
unsafe fn notify_multiple_hook_body(
    key: HANDLE,
    count: u32,
    subordinates: *const ObjectAttributes,
    event: HANDLE,
    apc: *const c_void,
    apc_ctx: *const c_void,
    iosb: *mut c_void,
    filter: u32,
    subtree: u8,
    buffer: *mut c_void,
    buffer_len: u32,
    asynchronous: u8,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::NotifyChangeMultipleKeys);
    let Some(tramp) = TRAMP_NOTIFY_MULTIPLE.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    let pass = || {
        tramp(
            key,
            count,
            subordinates,
            event,
            apc,
            apc_ctx,
            iosb,
            filter,
            subtree,
            buffer,
            buffer_len,
            asynchronous,
        )
    };
    if reg_bypass() {
        return pass();
    }
    let Some(_io) = ShimIoGuard::enter() else {
        return pass();
    };
    let args = crate::regnotify::Args {
        event,
        apc,
        apc_ctx,
        iosb,
        subtree: subtree != 0,
        asynchronous: asynchronous != 0,
        count,
    };
    match crate::regnotify::notify(&reg_real(), key as isize, &args) {
        crate::regnotify::Notify::Done(st) => st,
        crate::regnotify::Notify::Pass => pass(),
    }
}

/// `NtQuerySecurityObject` hook: a synthetic key answers the real key's (or nearest real
/// ancestor's) descriptor (`regkeys::query_security`); every other handle, real keys included,
/// gets the real call.
unsafe fn query_security_hook_body(
    handle: HANDLE,
    info: u32,
    sd: *mut c_void,
    length: u32,
    needed: *mut u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::QuerySecurityObject);
    let Some(tramp) = TRAMP_QUERY_SECURITY.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if !crate::regkeys::is_synthetic(handle as isize) || reg_bypass() {
        return tramp(handle, info, sd, length, needed);
    }
    let Some(_io) = ShimIoGuard::enter() else {
        return tramp(handle, info, sd, length, needed);
    };
    crate::regkeys::query_security(
        &reg_real(),
        tramp,
        handle as isize,
        info,
        sd,
        length,
        needed,
    )
}

/// `NtSetSecurityObject` hook: on a key the overlay serves (synthetic, or a real key on a
/// virtualised path) the change is checked, accepted and ignored (`regkeys::set_security`);
/// anything else gets the real call. A handle that cannot be resolved, or a call made while the
/// hook is bypassed with the overlay on, gets `STATUS_UNSUCCESSFUL`.
unsafe fn set_security_hook_body(handle: HANDLE, info: u32, sd: *const c_void) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::SetSecurityObject);
    let Some(tramp) = TRAMP_SET_SECURITY.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if !crate::regclient::enabled() {
        return tramp(handle, info, sd);
    }
    // With the overlay on, a security change this hook cannot examine (the shim's own call, or
    // no guard) is refused, as the write hooks refuse theirs: it may be on a virtualised key.
    let Some(_io) = reg_write_guard() else {
        return STATUS_UNSUCCESSFUL;
    };
    match crate::regkeys::set_security(&reg_real(), handle as isize, info, sd) {
        Some(st) => st,
        None => tramp(handle, info, sd),
    }
}

/// `NtSetInformationObject` hook: a synthetic key keeps its handle flags in its record
/// (`regkeys::set_handle_flags`); anything else gets the real call.
unsafe fn set_info_object_hook_body(
    handle: HANDLE,
    class: u32,
    info: *const c_void,
    length: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::SetInformationObject);
    let Some(tramp) = TRAMP_SET_INFO_OBJECT.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if !crate::regkeys::is_synthetic(handle as isize) || reg_bypass() {
        return tramp(handle, class, info, length);
    }
    match crate::regkeys::set_handle_flags(handle as isize, class, info, length) {
        Some(st) => st,
        None => tramp(handle, class, info, length),
    }
}

/// The body of a spec 3.6 hook. The overlay off goes to the real call. With it on, `refuse`
/// decides: `Some(status)` is returned, `None` makes the real call.
///
/// `modifies`: the call changes the real registry (Restore, Replace, Load*, Unload*, transacted
/// create/open). When the hook is bypassed with the overlay on (the shim's own call, or no
/// guard) such a call is refused with `STATUS_UNSUCCESSFUL`, as the write hooks refuse theirs
/// (`reg_write_guard`); the harmless ones (Save, Compress, Lock) still get the real call.
macro_rules! out_of_scope_body {
    ($(#[$attr:meta])* fn $body:ident($($arg:ident: $ty:ty),* $(,)?), $hook:ident, $tramp:ident,
     modifies = $modifies:expr, refuse = $refuse:expr;) => {
        $(#[$attr])*
        unsafe fn $body($($arg: $ty),*) -> NTSTATUS {
            let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::$hook);
            let Some(tramp) = $tramp.get() else {
                return STATUS_UNSUCCESSFUL;
            };
            if !crate::regclient::enabled() {
                return tramp($($arg),*);
            }
            let Some(_io) = reg_write_guard() else {
                if $modifies {
                    return STATUS_UNSUCCESSFUL;
                }
                return tramp($($arg),*);
            };
            if let Some(st) = $refuse {
                return st;
            }
            tramp($($arg),*)
        }
    };
}

/// A synthetic key handle: none of the spec 3.6 calls can act on it.
fn synthetic_key(key: HANDLE) -> Option<NTSTATUS> {
    crate::regkeys::is_synthetic(key as isize).then_some(STATUS_NOT_SUPPORTED)
}

/// What a real-modifying spec 3.6 call gets for a key the overlay may serve: `NOT_SUPPORTED` on
/// one it serves, `UNSUCCESSFUL` on one it cannot tell (fails closed, spec section 6), the real
/// call (`None`) otherwise.
fn refusal(serves: crate::regkeys::Serves) -> Option<NTSTATUS> {
    match serves {
        crate::regkeys::Serves::Yes => Some(STATUS_NOT_SUPPORTED),
        crate::regkeys::Serves::No => None,
        crate::regkeys::Serves::Invalid(st) => Some(st),
        crate::regkeys::Serves::Unresolvable => {
            crate::hookstats::note_reg_write_refused();
            Some(STATUS_UNSUCCESSFUL)
        }
    }
}

/// A key the overlay serves (synthetic, or real on a virtualised path): a call that would change
/// the real key through it is refused.
unsafe fn served_key(key: HANDLE) -> Option<NTSTATUS> {
    refusal(crate::regkeys::serves_handle(&reg_real(), key as isize))
}

/// A key name the overlay serves: a transacted open of it, or a hive loaded over or unloaded
/// from it, is refused.
unsafe fn served_target(oa: *const ObjectAttributes) -> Option<NTSTATUS> {
    refusal(crate::regkeys::serves_target(&reg_real(), oa))
}

out_of_scope_body! {
    #[allow(clippy::too_many_arguments)]
    fn create_key_tx_hook_body(
        key: *mut HANDLE,
        access: u32,
        oa: *const ObjectAttributes,
        title_index: u32,
        class: *const UnicodeString,
        options: u32,
        transaction: HANDLE,
        disposition: *mut u32,
    ), CreateKeyTransacted, TRAMP_CREATE_KEY_TX, modifies = true, refuse = served_target(oa);
}
out_of_scope_body! {
    fn open_key_tx_hook_body(
        key: *mut HANDLE,
        access: u32,
        oa: *const ObjectAttributes,
        transaction: HANDLE,
    ), OpenKeyTransacted, TRAMP_OPEN_KEY_TX, modifies = true, refuse = served_target(oa);
}
out_of_scope_body! {
    fn open_key_tx_ex_hook_body(
        key: *mut HANDLE,
        access: u32,
        oa: *const ObjectAttributes,
        options: u32,
        transaction: HANDLE,
    ), OpenKeyTransactedEx, TRAMP_OPEN_KEY_TX_EX, modifies = true, refuse = served_target(oa);
}
out_of_scope_body! {
    fn load_key_hook_body(target: *const ObjectAttributes, source: *const ObjectAttributes),
        LoadKey, TRAMP_LOAD_KEY, modifies = true, refuse = served_target(target);
}
out_of_scope_body! {
    fn load_key2_hook_body(
        target: *const ObjectAttributes,
        source: *const ObjectAttributes,
        flags: u32,
    ), LoadKey2, TRAMP_LOAD_KEY2, modifies = true, refuse = served_target(target);
}
out_of_scope_body! {
    #[allow(clippy::too_many_arguments)]
    fn load_key_ex_hook_body(
        target: *const ObjectAttributes,
        source: *const ObjectAttributes,
        flags: u32,
        a4: usize,
        a5: usize,
        a6: usize,
        a7: usize,
        a8: usize,
    ), LoadKeyEx, TRAMP_LOAD_KEY_EX, modifies = true, refuse = served_target(target);
}
out_of_scope_body! {
    #[allow(clippy::too_many_arguments)]
    fn load_key3_hook_body(
        target: *const ObjectAttributes,
        source: *const ObjectAttributes,
        flags: u32,
        a4: usize,
        a5: usize,
        a6: usize,
        a7: usize,
        a8: usize,
    ), LoadKey3, TRAMP_LOAD_KEY3, modifies = true, refuse = served_target(target);
}
out_of_scope_body! {
    fn unload_key_hook_body(target: *const ObjectAttributes),
        UnloadKey, TRAMP_UNLOAD_KEY, modifies = true, refuse = served_target(target);
}
out_of_scope_body! {
    fn unload_key2_hook_body(target: *const ObjectAttributes, a2: usize),
        UnloadKey2, TRAMP_UNLOAD_KEY2, modifies = true, refuse = served_target(target);
}
out_of_scope_body! {
    fn unload_key_ex_hook_body(target: *const ObjectAttributes, a2: usize),
        UnloadKeyEx, TRAMP_UNLOAD_KEY_EX, modifies = true, refuse = served_target(target);
}
// Saving, compressing and locking a real key read it or touch only the hive file: passed
// through on real keys, refused on synthetic ones only.
out_of_scope_body! {
    fn save_key_hook_body(key: HANDLE, file: HANDLE),
        SaveKey, TRAMP_SAVE_KEY, modifies = false, refuse = synthetic_key(key);
}
out_of_scope_body! {
    fn save_key_ex_hook_body(key: HANDLE, file: HANDLE, format: u32),
        SaveKeyEx, TRAMP_SAVE_KEY_EX, modifies = false, refuse = synthetic_key(key);
}
out_of_scope_body! {
    fn save_merged_hook_body(high: HANDLE, low: HANDLE, file: HANDLE),
        SaveMergedKeys, TRAMP_SAVE_MERGED, modifies = false,
        refuse = synthetic_key(high).or_else(|| synthetic_key(low));
}
out_of_scope_body! {
    fn compress_key_hook_body(key: HANDLE),
        CompressKey, TRAMP_COMPRESS_KEY, modifies = false, refuse = synthetic_key(key);
}
out_of_scope_body! {
    fn lock_registry_key_hook_body(key: HANDLE),
        LockRegistryKey, TRAMP_LOCK_REGISTRY_KEY, modifies = false, refuse = synthetic_key(key);
}
// Replacing and restoring write the real key: refused on every key the overlay serves.
out_of_scope_body! {
    fn replace_key_hook_body(
        new_file: *const ObjectAttributes,
        key: HANDLE,
        old_file: *const ObjectAttributes,
    ), ReplaceKey, TRAMP_REPLACE_KEY, modifies = true, refuse = served_key(key);
}
out_of_scope_body! {
    fn restore_key_hook_body(key: HANDLE, file: HANDLE, flags: u32),
        RestoreKey, TRAMP_RESTORE_KEY, modifies = true, refuse = served_key(key);
}

/// Path-based getattr via director OP_GETATTR when FUSE client is live.
/// `Some(...)` means the path is under the managed root — caller must not tramp
/// to the Steam tree on NOT_FOUND (seal under-root).
unsafe fn fuse_path_attr(path: &str) -> Option<Result<(bool, u64, i64), i32>> {
    let client = crate::fuse_client::global()?;
    let (root, vp) = client.route(path)?;
    Some(match client.getattr(root, &vp) {
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
    let attrs = if is_dir {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
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
    let tramp = match TRAMP_QIBN.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if in_hook_reenter() {
        return tramp(oa, iosb, info, length, class_raw);
    }
    if let Some(path) = match path_of(oa) {
        Ok(p) => p,
        Err(st) => return st,
    } {
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
                        // Classes 68 and 77 open with the file id. By handle
                        // it is the path's id; by name it must be the same
                        // number, not zero.
                        if matches!(class_raw, 68 | 77) {
                            if let Some(id) = path_file_id(&path) {
                                core::ptr::write_unaligned(info as *mut i64, id);
                            }
                        }
                        crate::hookstats::note_stat(&path, &format!("byname{class_raw}-ok"));
                        crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, n);
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
                        crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, n);
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
    let tramp = match TRAMP_QATTR.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if in_hook_reenter() {
        return tramp(oa, info);
    }
    if let Some(path) = match path_of(oa) {
        Ok(p) => p,
        Err(st) => return st,
    } {
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
                        (*info).file_attributes = if is_dir {
                            FILE_ATTRIBUTE_DIRECTORY
                        } else {
                            FILE_ATTRIBUTE_NORMAL
                        };
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
                        (*info).file_attributes = if is_dir {
                            FILE_ATTRIBUTE_DIRECTORY
                        } else {
                            FILE_ATTRIBUTE_NORMAL
                        };
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
    let tramp = match TRAMP_QFULL.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if in_hook_reenter() {
        return tramp(oa, info);
    }
    if let Some(path) = match path_of(oa) {
        Ok(p) => p,
        Err(st) => return st,
    } {
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
                        (*info).file_attributes = if is_dir {
                            FILE_ATTRIBUTE_DIRECTORY
                        } else {
                            FILE_ATTRIBUTE_NORMAL
                        };
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
                        (*info).file_attributes = if is_dir {
                            FILE_ATTRIBUTE_DIRECTORY
                        } else {
                            FILE_ATTRIBUTE_NORMAL
                        };
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
    let tramp = match TRAMP_DELETE.get() {
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
    let decoded = match path_of_tracked(oa) {
        Ok(d) => d,
        Err(st) => return st,
    };
    let _uncached_guard = decoded
        .as_ref()
        .is_some_and(|d| d.os_consulted)
        .then(vfs_redirect::UncachedScope::enter);
    let Some(path) = decoded.as_ref().map(|d| d.path.as_str()) else {
        // An undecodable delete is an undecodable open by another name: it
        // bypasses every decision we would have made. Recorded rather than
        // silently trampolined, so it shows up in the same place.
        crate::hookstats::note_undecodable(object_name_str(oa).as_deref());
        return tramp(oa);
    };

    if let Some(client) = crate::fuse_client::global() {
        if let Some((root, vp)) = client.route(path) {
            let vp = vp.as_str();
            client.names_changed(root, vp);
            crate::read_cache::invalidate_path(root.0, vp);
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
    let new_oa = match redirected_oa(oa, &nt) {
        Ok(o) => o,
        Err(st) => return st,
    };
    tramp(new_oa.as_ptr())
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

/// `FileCompletionInformation` — binds a handle to an I/O completion port.
const FILE_COMPLETION_INFORMATION: u32 = 30;

/// `NtSetInformationFile` hook. For director FUSE (pure-ring) virtual handles it
/// routes truncate (`FileEndOfFileInformation`), delete, and rename to the director
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
unsafe fn setinfo_hook_body(
    handle: HANDLE,
    iosb: *mut c_void,
    info: *mut c_void,
    length: u32,
    class: u32,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::SetInfo);
    let tramp = match TRAMP_SETINFO.get() {
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
            if let Some(f) = crate::fuse_synth::cache(handle as isize) {
                crate::read_cache::invalidate(&f);
            }
            if let (Some((fh, _, _, _, _)), Some(c)) = (
                crate::fuse_synth::lookup(handle as isize),
                crate::fuse_client::global(),
            ) {
                if eof >= 0 && c.truncate(fh, eof as u64).is_ok() {
                    crate::fuse_synth::set_size(handle as isize, eof as u64);
                    crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0);
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
                if let Some((root, src)) = c.route(&nt) {
                    c.names_changed(root, &src);
                    crate::read_cache::invalidate_path(root.0, &src);
                    let ok = if is_delete {
                        c.delete(root, &src).is_ok()
                    } else {
                        // The destination is a name being created, so it goes
                        // as the caller spelled it — which is also how a
                        // rename that changes only the letter case says what
                        // the new case is. See `FuseClient::vpath_as_spelled`.
                        let target = parse_rename_target(info, length);
                        match target.as_deref().and_then(|t| c.route_as_spelled(t)) {
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
                            Some((dst_root, dst)) if dst_root == root => {
                                c.names_changed(root, &vfs_core::fold(&dst));
                                crate::read_cache::invalidate_path(root.0, &vfs_core::fold(&dst));
                                let renamed = c.rename(root, &src, &dst).is_ok();
                                if renamed {
                                    // The handle follows the file: what it is
                                    // finally named, and its id, are the new
                                    // path's from here on.
                                    if let Some(t) = target {
                                        let nt = to_nt_path(&t);
                                        if let Ok(mut table) = PATH_TABLE.lock() {
                                            table.insert(handle as isize, nt.clone());
                                        }
                                        crate::fuse_synth::set_abs_path(handle as isize, nt);
                                    }
                                }
                                renamed
                            }
                            _ => false,
                        }
                    };
                    if ok {
                        crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0);
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
                        crate::engine::RenameOutcome::CrossRoot => return STATUS_UNSUCCESSFUL,
                        crate::engine::RenameOutcome::Declined => false,
                    },
                    None => false,
                }
            };
            if handled {
                // Suppress the real delete/rename; report success to the caller.
                crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0);
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

/// The final DOS path (`C:\dir\file`, stored spelling) of a synthetic handle:
/// the path it was opened as, re-spelled by [`FuseClient::final_path`]. Falls
/// back to the opened path itself, without its NT prefix, if the director
/// cannot be asked — a name in the caller's own spelling is still the right
/// file, where no name at all fails `GetFinalPathNameByHandleW` outright.
///
/// `None` only for a handle with no recorded path, which no open produces.
///
/// [`FuseClient::final_path`]: crate::fuse_client::FuseClient::final_path
fn synth_final_path(handle: HANDLE) -> Option<String> {
    let opened = crate::fuse_synth::abs_path(handle as isize)?;
    let named = crate::fuse_client::global().and_then(|c| c.final_path(&opened));
    Some(named.unwrap_or_else(|| {
        crate::fuse_client::strip_nt_device(&opened)
            .trim_end_matches('\\')
            .to_string()
    }))
}

/// The file id a synthetic handle reports: one per *file*, so that two
/// handles to one file agree (`std::filesystem::equivalent` and every "is
/// this the same file" check compare ids). It used to be the handle's own
/// value, which made a file unequal to itself.
///
/// From the root and the folded path under it — what the ring is asked for —
/// so every spelling of one file, through an alias of the root included,
/// gives one id, and no director round trip is spent on a query as common as
/// `GetFileInformationByHandle`.
fn synth_file_id(handle: HANDLE) -> i64 {
    crate::fuse_synth::abs_path(handle as isize)
        .and_then(|opened| path_file_id(&opened))
        .unwrap_or(handle as i64)
}

/// The file id of whatever is at `path` under a managed root — the number a
/// handle to it reports. `None` for a path under no root.
fn path_file_id(path: &str) -> Option<i64> {
    let (root, vpath) = crate::fuse_client::global()?.vpath_under_root(path)?;
    Some(vfs_core::finalname::path_id(&format!("{}:{vpath}", root.0)) as i64)
}

/// Whether this host names file objects `\??\C:\…` (Wine) or
/// `\Device\HarddiskVolumeN\…` (Windows), as the literal prefix
/// [`spoofed_object_name`] keys on.
///
/// A redirected *real* handle answers this itself: its own
/// `NtQueryObject` reply is consulted for the convention. A synthetic handle
/// is not a kernel object and has no reply to consult, so the question is put
/// once to a handle that is: the process's current-directory handle, which
/// the OS itself opened. If that cannot be asked, the host is identified
/// instead — Wine's ntdll exports `wine_get_version`, Windows' does not.
fn host_name_convention() -> &'static str {
    static CONVENTION: OnceLock<&'static str> = OnceLock::new();
    CONVENTION.get_or_init(|| {
        // SAFETY: reads this process's own PEB, and hands the trampoline a
        // buffer of the length it is told.
        #[allow(unsafe_code)]
        let probed = unsafe {
            match (TRAMP_QOBJ.get(), cwd_from_peb()) {
                (Some(tramp), Some((cwd, _))) => {
                    let mut scratch = vec![0u8; 2048];
                    let mut need = 0u32;
                    let st = tramp(
                        cwd as HANDLE,
                        OBJECT_NAME_INFORMATION,
                        scratch.as_mut_ptr().cast(),
                        scratch.len() as u32,
                        &mut need,
                    );
                    let n = u16::from_le_bytes([scratch[0], scratch[1]]) as usize;
                    let hdr = OBJECT_NAME_INFORMATION_HEADER;
                    if st >= 0 && n >= 8 && hdr + n <= scratch.len() {
                        let units: Vec<u16> = scratch[hdr..hdr + 16]
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|c| u16::from_le_bytes(*c))
                            .collect();
                        Some(String::from_utf16_lossy(&units))
                    } else {
                        None
                    }
                }
                _ => None,
            }
        };
        match probed {
            Some(name) if name.starts_with(r"\??\") => r"\??\",
            Some(name) if name.starts_with(r"\Device\") => r"\Device\",
            _ if host_is_wine() => r"\??\",
            _ => r"\Device\",
        }
    })
}

/// Whether ntdll is Wine's: it exports `wine_get_version`.
fn host_is_wine() -> bool {
    // SAFETY: both names are NUL-terminated; a missing module or export is a
    // null return, not a fault.
    #[allow(unsafe_code)]
    unsafe {
        let ntdll = windows_sys::Win32::System::LibraryLoader::GetModuleHandleA(
            c"ntdll.dll".as_ptr().cast(),
        );
        !ntdll.is_null()
            && windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                ntdll,
                c"wine_get_version".as_ptr().cast(),
            )
            .is_some()
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
    let Some((_, size, is_dir, pos, _append_only)) = crate::fuse_synth::lookup(handle as isize)
    else {
        return STATUS_INVALID_HANDLE;
    };
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
            (*bi).file_attributes = if is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            };
            (*bi)._reserved = 0;
            crate::ntbuf::iosb_set(
                iosb,
                STATUS_SUCCESS,
                core::mem::size_of::<FileBasicInformation>(),
            );
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
            crate::ntbuf::iosb_set(
                iosb,
                STATUS_SUCCESS,
                core::mem::size_of::<FileStandardInformation>(),
            );
            STATUS_SUCCESS
        }
        FILE_INTERNAL_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FileInternalInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            (*(info as *mut FileInternalInformation)).index_number = synth_file_id(handle);
            crate::ntbuf::iosb_set(
                iosb,
                STATUS_SUCCESS,
                core::mem::size_of::<FileInternalInformation>(),
            );
            STATUS_SUCCESS
        }
        FILE_POSITION_INFORMATION => {
            if (length as usize) < core::mem::size_of::<FilePositionInformation>() {
                return STATUS_BUFFER_OVERFLOW;
            }
            (*(info as *mut FilePositionInformation)).current_byte_offset = pos as i64;
            crate::ntbuf::iosb_set(
                iosb,
                STATUS_SUCCESS,
                core::mem::size_of::<FilePositionInformation>(),
            );
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
            (*ni).file_attributes = if is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            };
            crate::ntbuf::iosb_set(
                iosb,
                STATUS_SUCCESS,
                core::mem::size_of::<FileNetworkOpenInformation>(),
            );
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
            let attrs = if is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            };
            // Basic.FileAttributes @ 32
            core::ptr::write_unaligned(p.add(32) as *mut u32, attrs);
            // Standard.AllocationSize @ 40, EndOfFile @ 48, NumberOfLinks @ 56
            core::ptr::write_unaligned(p.add(40) as *mut i64, size as i64);
            core::ptr::write_unaligned(p.add(48) as *mut i64, size as i64);
            core::ptr::write_unaligned(p.add(56) as *mut u32, 1);
            // Standard.Directory (BOOLEAN) @ 61
            *p.add(61) = if is_dir { 1 } else { 0 };
            // Internal.IndexNumber @ 64
            core::ptr::write_unaligned(p.add(64) as *mut i64, synth_file_id(handle));
            // Position.CurrentByteOffset @ 80
            core::ptr::write_unaligned(p.add(80) as *mut i64, pos as i64);
            crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, PREFIX);
            STATUS_SUCCESS
        }
        FILE_NAME_INFORMATION | FILE_NORMALIZED_NAME_INFORMATION => {
            // The volume-relative name, for a file and for a directory alike,
            // in the stored spelling. `GetFinalPathNameByHandleW` builds its
            // answer from these two and from `NtQueryObject`'s name
            // (`qobj_hook_body`), and all three must describe one path — see
            // `qif_hook_body`. They are all cut from `synth_final_path`.
            //
            // There used to be no arm for either, on the reasoning that only
            // redirected real handles were ever asked for a name. They fell to
            // the catch-all below: success, nothing written.
            let Some(path) = synth_final_path(handle) else {
                return STATUS_INVALID_HANDLE;
            };
            // `FILE_NAME_INFORMATION` is a u32 byte length and then the name.
            // NT refuses a buffer smaller than the structure (8 bytes with
            // its one-character name field) outright; given one too small
            // for the whole name it writes the full length, as much of the
            // name as fits, and says overflow. A caller sizing a buffer
            // reads the length.
            if (length as usize) < 8 {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let name: Vec<u16> = vfs_core::finalname::volume_relative(&path)
                .encode_utf16()
                .collect();
            let fits = name.len().min((length as usize - 4) / 2);
            let p = info as *mut u8;
            core::ptr::write_unaligned(p as *mut u32, (name.len() * 2) as u32);
            for (i, unit) in name[..fits].iter().enumerate() {
                core::ptr::write_unaligned(p.add(4 + i * 2) as *mut u16, *unit);
            }
            let status = if fits == name.len() {
                STATUS_SUCCESS
            } else {
                STATUS_BUFFER_OVERFLOW
            };
            crate::ntbuf::iosb_set(iosb, status, 4 + fits * 2);
            status
        }
        FILE_ID_INFORMATION => {
            // VolumeSerialNumber u64 @0 | FileId (128 bits) @8 = 24. What
            // `GetFileInformationByHandleEx(FileIdInfo)` asks, which is how
            // `std::filesystem::equivalent` tells whether two paths are one
            // file. Unanswered, it compared two uninitialised buffers.
            const LEN: usize = 24;
            if (length as usize) < LEN {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let p = info as *mut u8;
            core::ptr::write_bytes(p, 0, LEN);
            core::ptr::write_unaligned(p as *mut u64, SYNTH_VOLUME_SERIAL);
            core::ptr::write_unaligned(p.add(8) as *mut i64, synth_file_id(handle));
            crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, LEN);
            STATUS_SUCCESS
        }
        FILE_STAT_INFORMATION => {
            // What `GetFileInformationByHandle` asks under current Wine
            // (GE-Proton 10), where it used to ask `FileAllInformation` — so
            // this is what Rust's `File::metadata` and `std::fs::read`'s size
            // hint now reach. Unanswered, it fell to the arm below, which
            // reports success without writing the buffer: the caller read its
            // own uninitialised stack as a file size and, in `fs::read`,
            // failed "out of memory" reserving that many bytes.
            //
            // Layout: FileId 0 | Creation 8 | LastAccess 16 | LastWrite 24 |
            // Change 32 | AllocationSize 40 | EndOfFile 48 | FileAttributes 56
            // | ReparseTag 60 | NumberOfLinks 64 | EffectiveAccess 68 = 72.
            const LEN: usize = 72;
            if (length as usize) < LEN {
                // What NT answers for a fixed-size class. `BUFFER_OVERFLOW`
                // means "the fixed part was written", which some callers
                // take as success — and nothing was.
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let p = info as *mut u8;
            core::ptr::write_bytes(p, 0, LEN);
            let attrs = if is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            };
            core::ptr::write_unaligned(p as *mut i64, synth_file_id(handle));
            for off in [8, 16, 24, 32] {
                core::ptr::write_unaligned(p.add(off) as *mut i64, SYNTH_FILETIME);
            }
            core::ptr::write_unaligned(p.add(40) as *mut i64, size as i64);
            core::ptr::write_unaligned(p.add(48) as *mut i64, size as i64);
            core::ptr::write_unaligned(p.add(56) as *mut u32, attrs);
            core::ptr::write_unaligned(p.add(64) as *mut u32, 1);
            // FILE_GENERIC_READ.
            core::ptr::write_unaligned(p.add(68) as *mut u32, 0x0012_0089);
            crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, LEN);
            STATUS_SUCCESS
        }
        FILE_ATTRIBUTE_TAG_INFORMATION => {
            // FileAttributes 0 | ReparseTag 4 = 8. Never a reparse point.
            const LEN: usize = 8;
            if (length as usize) < LEN {
                return STATUS_INFO_LENGTH_MISMATCH;
            }
            let p = info as *mut u8;
            let attrs = if is_dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_NORMAL
            };
            core::ptr::write_unaligned(p as *mut u32, attrs);
            core::ptr::write_unaligned(p.add(4) as *mut u32, 0);
            crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, LEN);
            STATUS_SUCCESS
        }
        _ => {
            crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0);
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
    let tramp = match TRAMP_QVOL.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        if class == FILE_FS_DEVICE_INFORMATION {
            if info.is_null() || (length as usize) < core::mem::size_of::<FileFsDeviceInformation>()
            {
                return STATUS_BUFFER_OVERFLOW;
            }
            let di = info as *mut FileFsDeviceInformation;
            (*di).device_type = FILE_DEVICE_DISK;
            (*di).characteristics = 0;
            crate::ntbuf::iosb_set(
                iosb,
                STATUS_SUCCESS,
                core::mem::size_of::<FileFsDeviceInformation>(),
            );
            return STATUS_SUCCESS;
        }
        // Soft-success for other volume classes (size/attr) with zeros.
        if !info.is_null() && length > 0 {
            core::ptr::write_bytes(info as *mut u8, 0, length as usize);
        }
        // `FileFsVolumeInformation` (class 1): VolumeCreationTime 0 |
        // VolumeSerialNumber 8 | VolumeLabelLength 12 | SupportsObjects 16 |
        // label. Zeros but for the serial number, which is the one
        // `FileIdInformation` reports for the same handle.
        if class == 1 && !info.is_null() && length >= 12 {
            core::ptr::write_unaligned(
                (info as *mut u8).add(8) as *mut u32,
                SYNTH_VOLUME_SERIAL as u32,
            );
        }
        crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, length as usize);
        return STATUS_SUCCESS;
    }
    tramp(handle, iosb, info, length, class)
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
    let tramp = match TRAMP_LOCK.get() {
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
            if exclusive != 0 {
                "lock-exclusive"
            } else {
                "lock-shared"
            },
            synth_path(handle).as_deref(),
        );
        crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0);
        if !event.is_null() {
            windows_sys::Win32::System::Threading::SetEvent(event);
        }
        return STATUS_SUCCESS;
    }
    tramp(
        handle,
        event,
        apc,
        apc_ctx,
        iosb,
        byte_offset,
        length,
        key,
        fail_immediately,
        exclusive,
    )
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
    let tramp = match TRAMP_UNLOCK.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        if !open_synth(handle) {
            return STATUS_INVALID_HANDLE;
        }
        crate::hookstats::note_synthetic_lock("unlock", synth_path(handle).as_deref());
        crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0);
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
    let tramp = match TRAMP_FLUSH.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        if !open_synth(handle) {
            return STATUS_INVALID_HANDLE;
        }
        crate::hookstats::note_synthetic_lock("flush", synth_path(handle).as_deref());
        crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, 0);
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
    let tramp = match TRAMP_QIF.get() {
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
            crate::ntbuf::iosb_set(iosb, status, r.bytes);
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
    let tramp = match TRAMP_QOBJ.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    // Cheapest rejections first, in order of how much of the world they let
    // past untouched. A class we do not answer is most of the traffic.
    // A synthetic registry key: its NT name, as the real key would report it, and the other
    // classes from `regkeys::query_object`.
    if crate::regkeys::is_synthetic(handle as isize) && crate::regclient::enabled() {
        if class != OBJECT_NAME_INFORMATION {
            return crate::regkeys::query_object(
                &reg_real(),
                tramp,
                handle as isize,
                class,
                info,
                length,
                ret_len,
            );
        }
        let name = match crate::regkeys::object_name(handle as isize) {
            None => return STATUS_INVALID_HANDLE,
            Some(Err(st)) => return st,
            Some(Ok(n)) => n,
        };
        return emit_object_name(&name, info, length, ret_len)
            .unwrap_or(STATUS_OBJECT_NAME_INVALID);
    }
    if class != OBJECT_NAME_INFORMATION {
        return tramp(handle, class, info, length, ret_len);
    }
    // A real key handle deleted or renamed through the overlay: the real key no longer names it.
    if crate::regclient::enabled() {
        match crate::regkeys::passthrough_name(handle as isize) {
            None => {}
            Some(Err(st)) => return st,
            Some(Ok(name)) => {
                return emit_object_name(&name, info, length, ret_len)
                    .unwrap_or(STATUS_OBJECT_NAME_INVALID);
            }
        }
    }
    // A synthetic handle — every file and directory the director serves — is
    // not a kernel object: the host has no name for it and the trampoline
    // fails on it. This used to fall through to exactly that failure, on the
    // reasoning that a convention must be measured, not guessed; and so
    // `GetFinalPathNameByHandleW`, which on Wine is this call and nothing
    // else, failed for every virtual file and directory, and
    // `std::filesystem::canonical` threw on them. The name is the handle's
    // final path, in the convention the host uses for real files.
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        let Some(path) = synth_final_path(handle) else {
            return STATUS_INVALID_HANDLE;
        };
        return match spoofed_object_name(host_name_convention(), &path, device_for_drive)
            .and_then(|name| emit_object_name(&name, info, length, ret_len))
        {
            Some(status) => status,
            // A path with no drive letter to name a device for, or too long
            // for a UNICODE_STRING: there is no honest name to give.
            None => STATUS_OBJECT_PATH_NOT_FOUND,
        };
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
    // (A synthetic handle never gets here: it was answered above.)
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

    match emit_object_name(&name, info, length, ret_len) {
        Some(status) => status,
        None => tramp(handle, class, info, length, ret_len),
    }
}

/// Write `name` as an `OBJECT_NAME_INFORMATION` into the caller's buffer,
/// following the too-small-buffer contract in [`qobj_hook_body`]'s doc.
/// `None` if the name cannot be described at all (it does not fit a
/// `UNICODE_STRING`), in which case nothing was written.
unsafe fn emit_object_name(
    name: &str,
    info: *mut c_void,
    length: u32,
    ret_len: *mut u32,
) -> Option<NTSTATUS> {
    let name16: Vec<u16> = name.encode_utf16().collect();
    let name_bytes = name16.len() * 2;
    // `UNICODE_STRING::MaximumLength` is a u16 and must cover the NUL. A name
    // that cannot be described in that field is one we must not try to emit.
    if name_bytes + 2 > u16::MAX as usize {
        return None;
    }
    let required = OBJECT_NAME_INFORMATION_HEADER + name_bytes + 2;
    // Set unconditionally and before any short-buffer return: both hosts fill
    // `ReturnLength` even when they write nothing at all.
    if !ret_len.is_null() {
        core::ptr::write_unaligned(ret_len, required as u32);
    }
    if info.is_null() || (length as usize) < OBJECT_NAME_INFORMATION_HEADER {
        return Some(STATUS_INFO_LENGTH_MISMATCH);
    }
    if (length as usize) < required {
        return Some(STATUS_BUFFER_OVERFLOW);
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
    Some(STATUS_SUCCESS)
}

/// `NtWriteFile` hook. For synthetic (fuse) write handles, forward the game's
/// buffer to the director overlay over the ring and complete the IRP; real handles
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
    let tramp = match TRAMP_WRITE.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        crate::hookstats::note_read_completion(!apc.is_null(), !event.is_null());
        let explicit = crate::ntbuf::explicit_offset(byte_offset);
        if let Some((fh, size, _is_dir, pos, append_only)) =
            crate::fuse_synth::lookup(handle as isize)
        {
            // Append-only access (FILE_APPEND_DATA without FILE_WRITE_DATA)
            // forces every write to the current end of file at the kernel
            // level, ignoring any offset the caller supplies — a real handle
            // enforces this itself; ours has to do it here.
            let off = if append_only {
                pos
            } else {
                explicit.unwrap_or(pos)
            };
            let want = length as usize;
            // The file is changing: the read cache drops it. (A write handle
            // already dropped it at open; this keeps the rule local.)
            if want > 0 {
                if let Some(f) = crate::fuse_synth::cache(handle as isize) {
                    crate::read_cache::invalidate(&f);
                }
            }
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
                        crate::ntbuf::iosb_set(iosb, STATUS_UNSUCCESSFUL, 0);
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
                crate::fuse_synth::grow_size(handle as isize, end);
            }
            crate::ntbuf::iosb_set(iosb, STATUS_SUCCESS, n);
            if !event.is_null() {
                windows_sys::Win32::System::Threading::SetEvent(event);
            }
            return STATUS_SUCCESS;
        }
        // Tagged synth handle with no table entry — never hand it to the real
        // NtWriteFile (mirrors read_hook).
        return STATUS_UNSUCCESSFUL;
    }
    tramp(
        handle,
        event,
        apc,
        apc_ctx,
        iosb,
        buffer,
        length,
        byte_offset,
        key,
    )
}

/// `NtReadFile` hook. Synthetic (fuse) handles are answered from the director
/// over the ring; real handles pass straight through. `ByteOffset` of NULL or
/// the "use current position" sentinel (-1/-2) means "current position".
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
    let tramp = match TRAMP_READ.get() {
        Some(t) => t,
        None => return STATUS_UNSUCCESSFUL,
    };
    if crate::fuse_synth::is_fuse_synth(handle as isize) {
        let explicit = crate::ntbuf::explicit_offset(byte_offset);
        if let Some(view) = crate::fuse_synth::lookup_read(handle as isize) {
            let (fh, size, pos) = (view.fh, view.size, view.position);
            let off = explicit.unwrap_or(pos);
            let want = length as usize;
            if off >= size {
                crate::ntbuf::iosb_set(iosb, STATUS_END_OF_FILE, 0);
                return STATUS_END_OF_FILE;
            }
            // Phase 1: fill the game's NtReadFile buffer in place (no intermediate tmp).
            let max = want.min((size - off) as usize);
            let n = if max == 0 || buffer.is_null() {
                0usize
            } else {
                // SAFETY: NtReadFile contract — buffer is writable for `length` bytes.
                let slice = unsafe { core::slice::from_raw_parts_mut(buffer as *mut u8, max) };
                // A small synchronous read of an immutable file is offered to
                // the read cache first. Not one that asked for completion by
                // APC or event (those keep exactly the path they had), and not
                // one whose handle's size has moved from what the cache was
                // told at open. A cache answer is `max` bytes, as the ring's
                // would be; `None` is the uncached read below, unchanged.
                let cached = match &view.cache {
                    Some(f) if apc.is_null() && event.is_null() && f.size() == Some(size) => {
                        crate::fuse_client::global().and_then(|c| c.read_cached(f, fh, off, slice))
                    }
                    _ => None,
                };
                match cached.ok_or(()).or_else(|()| {
                    crate::fuse_client::global()
                        .ok_or(vfs_protocol::ST_IO_ERROR)
                        .and_then(|c| c.read_fragmented(fh, off, slice))
                }) {
                    Ok(n) => n,
                    Err(_) => {
                        crate::ntbuf::iosb_set(iosb, STATUS_UNSUCCESSFUL, 0);
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
                crate::ntbuf::iosb_set(iosb, status, n);
                if !event.is_null() {
                    windows_sys::Win32::System::Threading::SetEvent(event);
                }
                return status;
            }
        }
        return STATUS_UNSUCCESSFUL;
    }
    tramp(
        handle,
        event,
        apc,
        apc_ctx,
        iosb,
        buffer,
        length,
        byte_offset,
        key,
    )
}

/// Back a VFS-served PE with a real file so the kernel can build the image
/// section, and return the trampoline's status.
///
/// `None` means the backing file could not be produced, and the caller should
/// fall back to the manual mapper.
///
/// The cache is keyed on the vpath and the image's bytes
/// ([`vfs_pe::image_cache_name`]), so an assembly loaded repeatedly
/// materialises once and a changed build of the same size never reuses it.
/// Files live under the shim's own temp directory and are left for the OS to
/// reclaim; they are content, not secrets, and the process
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
    // Named by the vpath and the image's bytes: a same-size different build
    // (a patch, an update) never reuses a stale copy.
    let name = vfs_pe::image_cache_name(&vpath, pe);
    let dir = std::env::temp_dir().join("vfs-pe-cache");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(&name);

    // Write once. A concurrent writer would be writing identical bytes, but a
    // reader must never see a half-written image, so build beside it and rename.
    let good =
        |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).ok() == Some(pe.len() as u64);
    if !good(&path) {
        let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
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
        if !vfs_pe::pe_looks_like_image(&pe) {
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
            Ok((base, img_size)) => {
                match crate::zipserve::register_mapped_image(base as usize, img_size as u64) {
                    Some(h) => {
                        if !section_handle.is_null() {
                            *section_handle = h as HANDLE;
                        }
                        STATUS_SUCCESS
                    }
                    None => STATUS_INVALID_FILE_FOR_SECTION,
                }
            }
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
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
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
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
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
    let tramp = match TRAMP_CREATE_SECTION.get() {
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
    let tramp = match TRAMP_MAP_VIEW.get() {
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
    let tramp = match TRAMP_UNMAP_VIEW.get() {
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
    let tramp = match TRAMP_CPIW.get() {
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
        token,
        app,
        cmd,
        proc_attr,
        thread_attr,
        inherit,
        forced,
        env,
        cur_dir_eff,
        si,
        pi,
        ptok,
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
/// mean "match everything" (`Ok(None)`). A string `ntbuf::us_units` rejects is `Err`.
unsafe fn wildcard_of(file_name: *const UnicodeString) -> Result<Option<String>, NTSTATUS> {
    Ok(crate::ntbuf::us_string(file_name)?.filter(|s| !(s.is_empty() || s == "*" || s == "*.*")))
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
    let tramp = match TRAMP_QDIREX.get() {
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
        &|| {
            tramp(
                handle, event, apc, apc_ctx, iosb, info, length, class_raw, flags, file_name,
            )
        },
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
    let tramp = match TRAMP_QDIR.get() {
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
                        wildcard_of(file_name).ok().flatten().as_deref(),
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
        // A wildcard NT's own capture refuses gets NT's answer.
        let wildcard = match wildcard_of(file_name) {
            Ok(w) => w,
            Err(st) => return st,
        };
        let routed =
            crate::fuse_client::global().and_then(|c| c.route(&dir_path).map(|hit| (c, hit)));
        match routed {
            Some((client, (root, vp))) => {
                let vp = vp.as_str();
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
                wildcard_of(file_name).ok().flatten().as_deref(),
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
    crate::ntbuf::iosb_set(iosb, status, result.bytes);
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `wildcard_of`: `*` and `*.*` and the empty string mean everything; a string NT's capture
    /// would reject (odd length, NULL buffer with a length) is an `Err`, where the odd one used
    /// to be a truncated pattern and the NULL one used to mean everything.
    #[test]
    fn wildcard_of_follows_the_shared_unicode_string_rule() {
        let enc = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
        let mut star = enc("*");
        let mut stardot = enc("*.*");
        let mut pat = enc("a*.esp");
        unsafe {
            assert_eq!(wildcard_of(core::ptr::null()), Ok(None));
            assert_eq!(wildcard_of(&us_raw(2, star.as_mut_ptr())), Ok(None));
            assert_eq!(wildcard_of(&us_raw(6, stardot.as_mut_ptr())), Ok(None));
            assert_eq!(wildcard_of(&us_raw(0, core::ptr::null_mut())), Ok(None));
            assert_eq!(
                wildcard_of(&us_raw(12, pat.as_mut_ptr())),
                Ok(Some("a*.esp".to_string()))
            );
            assert_eq!(
                wildcard_of(&us_raw(11, pat.as_mut_ptr())),
                Err(STATUS_OBJECT_NAME_INVALID)
            );
            assert_eq!(
                wildcard_of(&us_raw(4, core::ptr::null_mut())),
                Err(crate::ntdef::STATUS_ACCESS_VIOLATION)
            );
        }
    }

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
            spoofed_object_name(r"\Device\HarddiskVolume3\x", r"\??\C:\root\mod.esp", |_| {
                None
            }),
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
        for vpath in [
            r"\??\C:\root\mod.esp",
            r"\\?\C:\root\mod.esp",
            r"C:\root\mod.esp",
        ] {
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
            assert!(
                buf.iter().all(|b| *b == 0xAA),
                "class {class} wrote into a short buffer"
            );
        }
    }

    #[test]
    fn an_unknown_class_is_declined_so_the_caller_falls_through() {
        let mut buf = vec![0u8; 512];
        assert_eq!(fill(9999, &mut buf, false, 1), None);
    }

    /// The size a stat reports is the whole reason these classes are answered:
    /// a caller that sees zero bytes may skip the file without ever opening it.
    /// Offsets and sizes of the metadata classes we answer by path.
    ///
    /// These are ABI, not our choice: the caller allocated the buffer and reads
    /// the fields at fixed offsets. Writing `EndOfFile` at the wrong offset does
    /// not fail — it reports a file of the wrong size, or a size of zero, which
    /// a caller is free to treat as "not worth opening". That is silent, so it
    /// gets pinned down here.
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
            assert_eq!(
                i64_at(&buf, 40),
                SIZE as i64,
                "class {class} AllocationSize"
            );
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
}
