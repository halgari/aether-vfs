//! `CreateProcessInternalW`: injecting the shim into child processes.

use super::{TRAMP_CPIW, child_cwd_root};
use crate::inject::{inject_child, re_suspend};
use core::ffi::c_void;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW,
};

/// `kernelbase!CreateProcessInternalW` — the funnel under all CreateProcess*.
/// 12 params; only `flags` and `pi` are inspected/modified by the hook.
pub(super) type CreateProcessInternalWFn = unsafe extern "system" fn(
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
pub(super) static SELF_DLL: OnceLock<String> = OnceLock::new();

/// How long a spawning process waits for a child's shim to install its hooks
/// before resuming the child anyway (unvirtualized rather than hung).
const CHILD_READY_TIMEOUT_MS: u32 = 5_000;

/// `CreateProcessInternalW` hook: force the child to start suspended, dual-layer
/// inject (early payload + full shim), wait for hooks, then resume (unless the
/// caller asked for a suspended child). Best-effort — a failed inject or timeout
/// still resumes the child (unvirtualized rather than hung).
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn cpiw_hook_body(
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
