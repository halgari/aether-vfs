//! The ntdll detours. ALL `unsafe` in the crate lives here.
#![allow(unsafe_code)]

mod close;
mod dirquery;
mod entry;
mod file_attr;
mod file_info;
mod file_io;
mod file_mutate;
mod file_open;
mod handles;
mod install;
mod path;
#[cfg(test)]
mod test_support;
// @mods

use self::close::*;
use self::dirquery::*;
pub(crate) use self::entry::ShimIoGuard;
pub use self::entry::as_shim_io_for_tests;
pub use self::entry::contain_panic;
use self::entry::*;
use self::file_attr::*;
use self::file_info::*;
use self::file_io::*;
use self::file_mutate::*;
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

use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW,
};

use crate::engine::Engine;
use crate::inject::{inject_child, re_suspend};
use crate::ntdef::{
    NtCreateSectionFn, ObjectAttributes, SEC_IMAGE, STATUS_INVALID_FILE_FOR_SECTION,
    STATUS_INVALID_HANDLE, STATUS_NOT_SUPPORTED, STATUS_SECTION_TOO_BIG, STATUS_SUCCESS,
    STATUS_UNSUCCESSFUL, UnicodeString,
};

static ENGINE: OnceLock<Engine> = OnceLock::new();
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
