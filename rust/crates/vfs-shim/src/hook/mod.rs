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
mod process;
mod section;
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
use self::process::*;
use self::section::*;
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

use crate::engine::Engine;
use crate::ntdef::{ObjectAttributes, STATUS_NOT_SUPPORTED, STATUS_UNSUCCESSFUL, UnicodeString};

static ENGINE: OnceLock<Engine> = OnceLock::new();
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
