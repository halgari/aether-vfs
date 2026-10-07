//! The registry hooks.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{
    ShimIoGuard, TRAMP_CLOSE, TRAMP_COMPRESS_KEY, TRAMP_CREATE_KEY, TRAMP_CREATE_KEY_TX,
    TRAMP_DELETE_KEY, TRAMP_DELETE_VALUE, TRAMP_DUP, TRAMP_ENUM_KEY, TRAMP_ENUM_VALUE,
    TRAMP_FLUSH_KEY, TRAMP_LOAD_KEY, TRAMP_LOAD_KEY_EX, TRAMP_LOAD_KEY2, TRAMP_LOAD_KEY3,
    TRAMP_LOCK_REGISTRY_KEY, TRAMP_NOTIFY_KEY, TRAMP_NOTIFY_MULTIPLE, TRAMP_OPEN_KEY,
    TRAMP_OPEN_KEY_EX, TRAMP_OPEN_KEY_TX, TRAMP_OPEN_KEY_TX_EX, TRAMP_QOBJ, TRAMP_QUERY_KEY,
    TRAMP_QUERY_MULTIPLE, TRAMP_QUERY_SECURITY, TRAMP_QUERY_VALUE, TRAMP_RENAME_KEY,
    TRAMP_REPLACE_KEY, TRAMP_RESTORE_KEY, TRAMP_SAVE_KEY, TRAMP_SAVE_KEY_EX, TRAMP_SAVE_MERGED,
    TRAMP_SET_INFO_KEY, TRAMP_SET_INFO_OBJECT, TRAMP_SET_SECURITY, TRAMP_SET_VALUE,
    TRAMP_UNLOAD_KEY, TRAMP_UNLOAD_KEY_EX, TRAMP_UNLOAD_KEY2, in_hook_reenter,
};
use crate::ntdef::{ObjectAttributes, STATUS_NOT_SUPPORTED, STATUS_UNSUCCESSFUL, UnicodeString};
use core::ffi::c_void;
use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};

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

/// A write hook's answer, or the real call when the key is not one the overlay serves.
fn done_or(w: crate::regwrite::Write, pass: impl FnOnce() -> NTSTATUS) -> NTSTATUS {
    match w {
        crate::regwrite::Write::Done(st) => st,
        crate::regwrite::Write::Pass => pass(),
    }
}

/// The body of a read-shaped registry hook: the overlay off, or this thread inside the shim's own
/// work (`reg_bypass`), goes to the real call; otherwise `ShimIoGuard` is held for the whole call
/// (a registry or file call this thread makes while the hook works, the shim's own, goes straight
/// to ntdll) and `overlay` answers.
///
/// `|tramp|` (and `|tramp, pass|`) name the unhooked entry point (and a closure that calls it with
/// the hook's arguments) for `overlay`, an expression evaluated inside the generated `unsafe fn`
/// under its NT-pointer contract (hook/mod.rs).
macro_rules! reg_read_body {
    ($(#[$attr:meta])* fn $body:ident($($arg:ident: $ty:ty),* $(,)?),
     $hook:ident, $tramp:ident, |$t:ident| $overlay:expr;) => {
        reg_read_body! {
            $(#[$attr])* fn $body($($arg: $ty),*),
            $hook, $tramp, |$t, pass| $overlay;
        }
    };
    ($(#[$attr:meta])* fn $body:ident($($arg:ident: $ty:ty),* $(,)?),
     $hook:ident, $tramp:ident, |$t:ident, $pass:ident| $overlay:expr;) => {
        $(#[$attr])*
        pub(super) unsafe fn $body($($arg: $ty),*) -> NTSTATUS {
            let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::$hook);
            let Some($t) = $tramp.get() else {
                return STATUS_UNSUCCESSFUL;
            };
            #[allow(unused_variables)]
            let $pass = || {
                // SAFETY: the original NT function, called with valid NT arguments.
                unsafe { $t($($arg),*) }
            };
            if reg_bypass() {
                return $pass();
            }
            let Some(_io) = ShimIoGuard::enter() else {
                return $pass();
            };
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            unsafe { $overlay }
        }
    };
}

/// The body of a write-shaped registry hook. The overlay off goes to the real call. With it on, a
/// write made while the hook is bypassed (the shim's own work: `ShimIoGuard` is already held) is
/// refused with `STATUS_UNSUCCESSFUL`, never made for real. Otherwise `overlay` (an expression
/// under the generated `unsafe fn`'s NT-pointer contract) answers a [`Write`], and `Write::Pass`
/// makes the real call.
///
/// [`Write`]: crate::regwrite::Write
macro_rules! reg_write_body {
    ($(#[$attr:meta])* fn $body:ident($($arg:ident: $ty:ty),* $(,)?),
     $hook:ident, $tramp:ident, $overlay:expr;) => {
        $(#[$attr])*
        pub(super) unsafe fn $body($($arg: $ty),*) -> NTSTATUS {
            let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::$hook);
            let Some(tramp) = $tramp.get() else {
                return STATUS_UNSUCCESSFUL;
            };
            if !crate::regclient::enabled() {
                // SAFETY: the original NT function, called with valid NT arguments.
                return unsafe { tramp($($arg),*) };
            }
            let Some(_io) = ShimIoGuard::enter() else {
                return STATUS_UNSUCCESSFUL;
            };
            let _ws = crate::regclient::WriteScope::enter();
            // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
            done_or(unsafe { $overlay }, || {
                // SAFETY: the original NT function, called with valid NT arguments.
                unsafe { tramp($($arg),*) }
            })
        }
    };
}

reg_read_body! {
    /// `NtOpenKey` hook. See `regkeys::open_or_create`.
    fn open_key_hook_body(
        key: *mut HANDLE,
        access: u32,
        oa: *const ObjectAttributes,
    ),
    OpenKey, TRAMP_OPEN_KEY, |tramp| {
        crate::regkeys::open_or_create(
            &reg_real(),
            key,
            access,
            oa,
            crate::regkeys::Call::Open,
            &mut |oa| tramp(key, access, oa),
        )
        .status
    };
}

reg_read_body! {
    /// `NtOpenKeyEx` hook. See `regkeys::open_or_create`.
    fn open_key_ex_hook_body(
        key: *mut HANDLE,
        access: u32,
        oa: *const ObjectAttributes,
        options: u32,
    ),
    OpenKeyEx, TRAMP_OPEN_KEY_EX, |tramp| {
        crate::regkeys::open_or_create(
            &reg_real(),
            key,
            access,
            oa,
            crate::regkeys::Call::Open,
            &mut |oa| tramp(key, access, oa, options),
        )
        .status
    };
}

/// `NtCreateKey` hook. With the overlay on, the real `NtCreateKey` is never called: a key that
/// exists for real is opened (`NtOpenKeyEx` trampoline, the caller's access and open options)
/// and reported as `REG_OPENED_EXISTING_KEY`; one that does not is created in the overlay.
/// `TitleIndex` and `Class` are not modelled by the overlay and are ignored for its keys.
pub(super) unsafe fn create_key_hook_body(
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
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(key, access, oa, title_index, class, options, disposition) };
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
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    let out = unsafe {
        crate::regkeys::open_or_create(
            &reg_real(),
            key,
            access,
            oa,
            crate::regkeys::Call::Create { options },
            &mut |oa| open_ex(key, access, oa, open_options),
        )
    };
    if out.status >= 0 && !disposition.is_null() {
        // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
        unsafe {
            *disposition = out.disposition;
        }
    }
    out.status
}

/// `NtDuplicateObject` hook: duplicates of tracked key handles stay tracked. See
/// `regkeys::duplicate`.
pub(super) unsafe fn dup_hook_body(
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
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe {
            tramp(
                src_process,
                src,
                dst_process,
                dst,
                access,
                attributes,
                options,
            )
        };
    }
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    match unsafe {
        crate::regkeys::duplicate(
            &reg_real(),
            src_process,
            src,
            dst_process,
            dst,
            access,
            attributes,
            options,
        )
    } {
        Some(st) => st,
        // SAFETY: the original NT function, called with valid NT arguments.
        None => unsafe {
            tramp(
                src_process,
                src,
                dst_process,
                dst,
                access,
                attributes,
                options,
            )
        },
    }
}

reg_read_body! {
    /// `NtQueryKey` hook. See `regquery::query_key`.
    fn query_key_hook_body(
        key: HANDLE,
        class: u32,
        info: *mut c_void,
        length: u32,
        ret_len: *mut u32,
    ),
    QueryKey, TRAMP_QUERY_KEY, |tramp| crate::regquery::query_key(&reg_real(), key as isize, class, info, length, ret_len);
}

reg_read_body! {
    /// `NtEnumerateKey` hook. See `regquery::enumerate_key`.
    fn enum_key_hook_body(
        key: HANDLE,
        index: u32,
        class: u32,
        info: *mut c_void,
        length: u32,
        ret_len: *mut u32,
    ),
    EnumerateKey, TRAMP_ENUM_KEY, |tramp| crate::regquery::enumerate_key(&reg_real(), key as isize, index, class, info, length, ret_len);
}

reg_read_body! {
    /// `NtQueryValueKey` hook. See `regquery::query_value_key`.
    fn query_value_hook_body(
        key: HANDLE,
        name: *const UnicodeString,
        class: u32,
        info: *mut c_void,
        length: u32,
        ret_len: *mut u32,
    ),
    QueryValueKey, TRAMP_QUERY_VALUE, |tramp| crate::regquery::query_value_key(&reg_real(), key as isize, name, class, info, length, ret_len);
}

reg_read_body! {
    /// `NtEnumerateValueKey` hook. See `regquery::enumerate_value_key`.
    fn enum_value_hook_body(
        key: HANDLE,
        index: u32,
        class: u32,
        info: *mut c_void,
        length: u32,
        ret_len: *mut u32,
    ),
    EnumerateValueKey, TRAMP_ENUM_VALUE, |tramp| crate::regquery::enumerate_value_key(&reg_real(), key as isize, index, class, info, length, ret_len);
}

reg_read_body! {
    /// `NtQueryMultipleValueKey` hook. See `regquery::query_multiple_value_key`.
    fn query_multiple_hook_body(
        key: HANDLE,
        entries: *mut c_void,
        count: u32,
        buffer: *mut c_void,
        buffer_len: *mut u32,
        required: *mut u32,
    ),
    QueryMultipleValueKey, TRAMP_QUERY_MULTIPLE, |tramp| crate::regquery::query_multiple_value_key(
        &reg_real(),
        key as isize,
        entries,
        count,
        buffer,
        buffer_len,
        required,
    );
}

reg_write_body! {
    /// `NtSetValueKey` hook. With the overlay on, a write on a virtualised key goes to the director
    /// (`regwrite::set_value_key`) and never to the real key; `TitleIndex` is ignored, as Windows
    /// ignores it.
    fn set_value_key_hook_body(
        key: HANDLE,
        name: *const UnicodeString,
        title_index: u32,
        ty: u32,
        data: *const c_void,
        size: u32,
    ),
    SetValueKey, TRAMP_SET_VALUE, crate::regwrite::set_value_key(&reg_real(), key as isize, name, ty, data, size);
}

reg_write_body! {
    /// `NtDeleteValueKey` hook. See `regwrite::delete_value_key`.
    fn delete_value_key_hook_body(
        key: HANDLE,
        name: *const UnicodeString,
    ),
    DeleteValueKey, TRAMP_DELETE_VALUE, crate::regwrite::delete_value_key(&reg_real(), key as isize, name);
}

reg_write_body! {
    /// `NtDeleteKey` hook. See `regwrite::delete_key`.
    fn delete_key_hook_body(
        key: HANDLE,
    ),
    DeleteKey, TRAMP_DELETE_KEY, crate::regwrite::delete_key(&reg_real(), key as isize);
}

reg_write_body! {
    /// `NtRenameKey` hook. See `regwrite::rename_key`.
    fn rename_key_hook_body(
        key: HANDLE,
        new_name: *const UnicodeString,
    ),
    RenameKey, TRAMP_RENAME_KEY, crate::regwrite::rename_key(&reg_real(), key as isize, new_name);
}

reg_write_body! {
    /// `NtSetInformationKey` hook. See `regwrite::set_information_key`.
    fn set_info_key_hook_body(
        key: HANDLE,
        class: u32,
        info: *const c_void,
        length: u32,
    ),
    SetInformationKey, TRAMP_SET_INFO_KEY, crate::regwrite::set_information_key(&reg_real(), key as isize, class, info, length);
}

reg_read_body! {
    /// `NtFlushKey` hook. See `regwrite::flush_key`. A flush writes nothing the caller did not
    /// already write, so the shim's own (re-entrant) calls pass through like the read hooks'.
    fn flush_key_hook_body(
        key: HANDLE,
    ),
    FlushKey, TRAMP_FLUSH_KEY, |tramp| {
        let _ws = crate::regclient::WriteScope::enter();
        done_or(crate::regwrite::flush_key(&reg_real(), key as isize), || tramp(key))
    };
}

reg_read_body! {
    /// `NtNotifyChangeKey` hook. A key the overlay serves gets an overlay waiter
    /// (`regnotify::notify`); anything else the real call.
    #[allow(clippy::too_many_arguments)]
    fn notify_key_hook_body(
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
    ),
    NotifyChangeKey, TRAMP_NOTIFY_KEY, |tramp, pass| {
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
    };
}

reg_read_body! {
    /// `NtNotifyChangeMultipleKeys` hook: as `NtNotifyChangeKey` for the master key; subordinate
    /// keys on a key the overlay serves are `STATUS_NOT_SUPPORTED`.
    #[allow(clippy::too_many_arguments)]
    fn notify_multiple_hook_body(
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
    ),
    NotifyChangeMultipleKeys, TRAMP_NOTIFY_MULTIPLE, |tramp, pass| {
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
    };
}

/// `NtQuerySecurityObject` hook: a synthetic key answers the real key's (or nearest real
/// ancestor's) descriptor (`regkeys::query_security`); every other handle, real keys included,
/// gets the real call.
pub(super) unsafe fn query_security_hook_body(
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
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(handle, info, sd, length, needed) };
    }
    let Some(_io) = ShimIoGuard::enter() else {
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(handle, info, sd, length, needed) };
    };
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    unsafe {
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
}

/// `NtSetSecurityObject` hook: on a key the overlay serves (synthetic, or a real key on a
/// virtualised path) the change is checked, accepted and ignored (`regkeys::set_security`);
/// anything else gets the real call. A handle that cannot be resolved, or a call made while the
/// hook is bypassed with the overlay on, gets `STATUS_UNSUCCESSFUL`.
pub(super) unsafe fn set_security_hook_body(
    handle: HANDLE,
    info: u32,
    sd: *const c_void,
) -> NTSTATUS {
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::SetSecurityObject);
    let Some(tramp) = TRAMP_SET_SECURITY.get() else {
        return STATUS_UNSUCCESSFUL;
    };
    if !crate::regclient::enabled() {
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(handle, info, sd) };
    }
    // With the overlay on, a security change this hook cannot examine (the shim's own call, or
    // no guard) is refused, as the write hooks refuse theirs: it may be on a virtualised key.
    let Some(_io) = ShimIoGuard::enter() else {
        return STATUS_UNSUCCESSFUL;
    };
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    match unsafe { crate::regkeys::set_security(&reg_real(), handle as isize, info, sd) } {
        Some(st) => st,
        // SAFETY: the original NT function, called with valid NT arguments.
        None => unsafe { tramp(handle, info, sd) },
    }
}

/// `NtSetInformationObject` hook: a synthetic key keeps its handle flags in its record
/// (`regkeys::set_handle_flags`); anything else gets the real call.
pub(super) unsafe fn set_info_object_hook_body(
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
        // SAFETY: the original NT function, called with valid NT arguments.
        return unsafe { tramp(handle, class, info, length) };
    }
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    match unsafe { crate::regkeys::set_handle_flags(handle as isize, class, info, length) } {
        Some(st) => st,
        // SAFETY: the original NT function, called with valid NT arguments.
        None => unsafe { tramp(handle, class, info, length) },
    }
}

/// The body of a spec 3.6 hook. The overlay off goes to the real call. With it on, `refuse`
/// decides: `Some(status)` is returned, `None` makes the real call.
///
/// `modifies`: the call changes the real registry (Restore, Replace, Load*, Unload*, transacted
/// create/open). When the hook is bypassed with the overlay on (the shim's own call, or no
/// guard) such a call is refused with `STATUS_UNSUCCESSFUL`, as the write hooks refuse theirs
/// (`ShimIoGuard::enter` fails); the harmless ones (Save, Compress, Lock) still get the real call.
///
/// `refuse` is evaluated inside the generated `unsafe fn` but is not itself in an unsafe block:
/// an invocation whose `refuse` calls an `unsafe fn` (`served_key`, `served_target`) wraps that
/// call, with the same NT-pointer contract as the generated fn (hook/mod.rs).
macro_rules! out_of_scope_body {
    ($(#[$attr:meta])* fn $body:ident($($arg:ident: $ty:ty),* $(,)?), $hook:ident, $tramp:ident,
     modifies = $modifies:expr, refuse = $refuse:expr;) => {
        $(#[$attr])*
        pub(super) unsafe fn $body($($arg: $ty),*) -> NTSTATUS {
            let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::$hook);
            let Some(tramp) = $tramp.get() else {
                return STATUS_UNSUCCESSFUL;
            };
            if !crate::regclient::enabled() {
                // SAFETY: the original NT function, called with valid NT arguments.
                return unsafe { tramp($($arg),*) };
            }
            let Some(_io) = ShimIoGuard::enter() else {
                if $modifies {
                    return STATUS_UNSUCCESSFUL;
                }
                // SAFETY: the original NT function, called with valid NT arguments.
                return unsafe { tramp($($arg),*) };
            };
            if let Some(st) = $refuse {
                return st;
            }
            // SAFETY: the original NT function, called with valid NT arguments.
            unsafe { tramp($($arg),*) }
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
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    refusal(unsafe { crate::regkeys::serves_handle(&reg_real(), key as isize) })
}

/// A key name the overlay serves: a transacted open of it, or a hive loaded over or unloaded
/// from it, is refused.
unsafe fn served_target(oa: *const ObjectAttributes) -> Option<NTSTATUS> {
    // SAFETY: same NT-pointer contract as this fn (hook/mod.rs).
    refusal(unsafe { crate::regkeys::serves_target(&reg_real(), oa) })
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
    ), CreateKeyTransacted, TRAMP_CREATE_KEY_TX, modifies = true, refuse = unsafe { served_target(oa) };
}
out_of_scope_body! {
    fn open_key_tx_hook_body(
        key: *mut HANDLE,
        access: u32,
        oa: *const ObjectAttributes,
        transaction: HANDLE,
    ), OpenKeyTransacted, TRAMP_OPEN_KEY_TX, modifies = true, refuse = unsafe { served_target(oa) };
}
out_of_scope_body! {
    fn open_key_tx_ex_hook_body(
        key: *mut HANDLE,
        access: u32,
        oa: *const ObjectAttributes,
        options: u32,
        transaction: HANDLE,
    ), OpenKeyTransactedEx, TRAMP_OPEN_KEY_TX_EX, modifies = true, refuse = unsafe { served_target(oa) };
}
out_of_scope_body! {
    fn load_key_hook_body(target: *const ObjectAttributes, source: *const ObjectAttributes),
        LoadKey, TRAMP_LOAD_KEY, modifies = true, refuse = unsafe { served_target(target) };
}
out_of_scope_body! {
    fn load_key2_hook_body(
        target: *const ObjectAttributes,
        source: *const ObjectAttributes,
        flags: u32,
    ), LoadKey2, TRAMP_LOAD_KEY2, modifies = true, refuse = unsafe { served_target(target) };
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
    ), LoadKeyEx, TRAMP_LOAD_KEY_EX, modifies = true, refuse = unsafe { served_target(target) };
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
    ), LoadKey3, TRAMP_LOAD_KEY3, modifies = true, refuse = unsafe { served_target(target) };
}
out_of_scope_body! {
    fn unload_key_hook_body(target: *const ObjectAttributes),
        UnloadKey, TRAMP_UNLOAD_KEY, modifies = true, refuse = unsafe { served_target(target) };
}
out_of_scope_body! {
    fn unload_key2_hook_body(target: *const ObjectAttributes, a2: usize),
        UnloadKey2, TRAMP_UNLOAD_KEY2, modifies = true, refuse = unsafe { served_target(target) };
}
out_of_scope_body! {
    fn unload_key_ex_hook_body(target: *const ObjectAttributes, a2: usize),
        UnloadKeyEx, TRAMP_UNLOAD_KEY_EX, modifies = true, refuse = unsafe { served_target(target) };
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
    ), ReplaceKey, TRAMP_REPLACE_KEY, modifies = true, refuse = unsafe { served_key(key) };
}
out_of_scope_body! {
    fn restore_key_hook_body(key: HANDLE, file: HANDLE, flags: u32),
        RestoreKey, TRAMP_RESTORE_KEY, modifies = true, refuse = unsafe { served_key(key) };
}
