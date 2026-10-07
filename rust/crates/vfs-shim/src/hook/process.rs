//! `CreateProcessInternalW`: injecting the shim into child processes.
#![deny(unsafe_op_in_unsafe_fn)]

use super::{child_cwd_root, TRAMP_CPIW};
use crate::child::{child_ready_timeout_ms, inject_child, ChildInjectError};
use core::ffi::c_void;
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{CloseHandle, SetLastError, ERROR_PROCESS_ABORTED, HANDLE};
use windows_sys::Win32::System::Threading::{
    QueryFullProcessImageNameW, ResumeThread, TerminateProcess, WaitForSingleObject,
    CREATE_SUSPENDED, PROCESS_INFORMATION, STARTUPINFOW,
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

/// `CreateProcessInternalW` hook: force the child to start suspended, inject
/// the shim (which bootstraps inside its `LoadLibrary`), then resume, unless the
/// caller asked for a suspended child, which it gets still suspended.
///
/// **Fails closed.** Every child this hook creates is injected, and if that
/// fails for any reason (no shim DLL path, a LoadLibrary failure, the child's shim reporting failure, the child dying, or no ready
/// signal within the launch's ready timeout) the child is killed, its handles
/// are closed, and this call returns `FALSE` with `ERROR_PROCESS_ABORTED`. The
/// child is never released un-virtualised. There is no list of children the
/// shim deliberately leaves alone: a `CreateProcess` under the shim is either a
/// virtualised child or an error. See docs/shim-invariants.md, "Child
/// processes fail closed".
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
    let _hs = crate::hookstats::Timed::new(crate::hookstats::Hook::Cpiw);
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
    // SAFETY: the original NT function, called with valid NT arguments.
    let r = unsafe {
        tramp(
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
        )
    };
    if r != 0 && !pi.is_null() {
        // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
        let pid = unsafe { (*pi).dwProcessId };
        // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
        let hprocess = unsafe { (*pi).hProcess };
        // SAFETY: raw access under the NT-pointer contract (hook/mod.rs).
        let hthread = unsafe { (*pi).hThread };
        // A patched child loads the shim through its own import table, and the
        // loader refuses to start it without one, so it is not injected:
        // injecting would load a second copy.
        if child_imports_shim(hprocess) {
            if !caller_suspended {
                // SAFETY: FFI call with valid arguments.
                unsafe { ResumeThread(hthread) };
            }
            return r;
        }
        match inject_child(
            hprocess,
            hthread,
            pid,
            SELF_DLL.get().map(String::as_str),
            child_ready_timeout_ms(),
        ) {
            Ok(()) => {
                // Injection never resumed the primary thread.
                if !caller_suspended {
                    // SAFETY: FFI call with valid arguments.
                    unsafe { ResumeThread(hthread) };
                }
            }
            Err(why) => {
                // SAFETY: `pi` and `ptok` are the caller's out-parameters from
                // the call that just succeeded; `app`/`cmd` are the caller's strings.
                unsafe { refuse_child(pi, ptok, app, cmd, why) };
                return 0;
            }
        }
    }
    r
}

/// Whether the child's EXE imports the shim first (staging patched it).
///
/// Read from the real disk, not through the VFS: staging's copy is mounted
/// *below* the curated content, so the VFS answers the image's path with the
/// original, unpatched EXE.
fn child_imports_shim(process: HANDLE) -> bool {
    // `None` means this thread is already inside the shim's own I/O, where
    // every file call goes to the real disk anyway.
    let _real_disk = super::entry::ShimIoGuard::enter();
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    // SAFETY: a process handle from `CreateProcess`, into a buffer of `len` units.
    let ok = unsafe { QueryFullProcessImageNameW(process, 0, buf.as_mut_ptr(), &mut len) };
    ok != 0 && vfs_inject::exe_imports_shim(&String::from_utf16_lossy(&buf[..len as usize]))
}

/// Kill a child that could not be injected and make the `CreateProcess` call
/// that made it fail.
///
/// The process is terminated and given a moment to go away (termination is
/// asynchronous, and a caller that sees `FALSE` expects no child to be left),
/// both handles are closed and cleared so a careless caller cannot use them,
/// a token handed back through `ptok` is closed, and the failure is counted by
/// reason in the hook stats (ungated).
///
/// # Safety
/// `pi` must point to the `PROCESS_INFORMATION` the successful call filled,
/// with handles not yet closed; `ptok` is null or the call's token out-pointer.
unsafe fn refuse_child(
    pi: *mut PROCESS_INFORMATION,
    ptok: *mut HANDLE,
    app: *const u16,
    cmd: *const u16,
    why: ChildInjectError,
) {
    // SAFETY: per the contract above; `app`/`cmd` are the caller's NUL-terminated
    // strings or null.
    let image = unsafe { child_image(app, cmd) };
    // SAFETY: per the contract above.
    unsafe {
        let p = &mut *pi;
        TerminateProcess(p.hProcess, 1);
        WaitForSingleObject(p.hProcess, 5_000);
        CloseHandle(p.hThread);
        CloseHandle(p.hProcess);
        p.hThread = core::ptr::null_mut();
        p.hProcess = core::ptr::null_mut();
        p.dwProcessId = 0;
        p.dwThreadId = 0;
        if !ptok.is_null() && !(*ptok).is_null() {
            CloseHandle(*ptok);
            *ptok = core::ptr::null_mut();
        }
    }
    crate::hookstats::note_child_inject_refused(why.label());
    log_refusal(&image, why.label());
    // Last: the file I/O above would overwrite it.
    // SAFETY: FFI call with a valid argument.
    unsafe { SetLastError(ERROR_PROCESS_ABORTED) };
}

/// Append `<image> <reason>` to the file `VFS_CHILD_REFUSED_LOG` names, which
/// the launcher reads. Best-effort: the kill and the failed call do not depend
/// on it.
fn log_refusal(image: &str, reason: &str) {
    use std::io::Write;
    let Some(path) = vfs_env::text(vfs_env::CHILD_REFUSED_LOG) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        // One write, so concurrent refusals do not interleave within a line.
        let _ = f.write_all(format!("{image} {reason}\n").as_bytes());
    }
}

/// The program a `CreateProcess` call names: `app` if given, else the first
/// token of the command line (a quoted token whole). Capped, and a newline
/// would break the one-line-per-refusal format, so those become spaces.
///
/// # Safety
/// `app` and `cmd` are null or point to NUL-terminated UTF-16.
unsafe fn child_image(app: *const u16, cmd: *const u16) -> String {
    const CAP: usize = 4096;
    let read = |p: *const u16| -> Vec<u16> {
        let mut v = Vec::new();
        if !p.is_null() {
            // SAFETY: per the contract, read up to the NUL or the cap.
            unsafe {
                while v.len() < CAP && *p.add(v.len()) != 0 {
                    v.push(*p.add(v.len()));
                }
            }
        }
        v
    };
    let mut w = read(app);
    if w.is_empty() {
        let c = read(cmd);
        w = match c.first() {
            Some(&q) if q == u16::from(b'"') => c[1..]
                .iter()
                .copied()
                .take_while(|&x| x != u16::from(b'"'))
                .collect(),
            _ => c
                .iter()
                .copied()
                .take_while(|&x| x != u16::from(b' '))
                .collect(),
        };
    }
    String::from_utf16_lossy(&w).replace(['\r', '\n'], " ")
}
