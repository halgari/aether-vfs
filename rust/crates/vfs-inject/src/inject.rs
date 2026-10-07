//! Win32 launch and injection: start the target, or start it suspended and
//! `LoadLibrary` the shim into it on a remote thread.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    GetThreadContext, SetThreadContext, WriteProcessMemory, CONTEXT,
};

const CONTEXT_FULL: u32 = 0x0010_000B;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CreateRemoteThread, GetExitCodeProcess, ResumeThread, TerminateProcess,
    WaitForSingleObject, CREATE_SUSPENDED, INFINITE, LPTHREAD_START_ROUTINE, PROCESS_INFORMATION,
    STARTUPINFOW,
};

use crate::{InjectError, RunConfig};

/// The primary thread's stack for a virtualised process. The shim adds frames
/// to every intercepted call, and the stock 1 MiB stack overflows under that
/// (`0xC00000FD`). An import-patched exe carries it in its header; an injected
/// process gets it from [`expand_primary_stack`].
pub const PRIMARY_STACK_BYTES: usize = 16 * 1024 * 1024;

/// `STACK_SIZE_PARAM_IS_A_RESERVATION`: `CreateRemoteThread`'s size is the
/// reservation, not the commit.
const STACK_SIZE_PARAM_IS_A_RESERVATION: u32 = 0x0001_0000;

/// The exit code of `process` if it has already exited.
///
/// # Safety
/// `process` must be a live process handle with `SYNCHRONIZE` and
/// `PROCESS_QUERY_LIMITED_INFORMATION` access (a `CreateProcessW` handle).
unsafe fn exited(process: HANDLE) -> Option<u32> {
    if WaitForSingleObject(process, 0) != 0 {
        return None;
    }
    let mut code = 0u32;
    (GetExitCodeProcess(process, &mut code) != 0).then_some(code)
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Grow the *primary* suspended thread's stack to `stack_reserve` bytes.
///
/// # Safety
/// `process` and `primary` must be the handles of a suspended process that
/// has not run, and its primary thread, with the rights `CreateProcess` gives.
///
/// Keeps CreateProcess image = Steam library path (DRM) while avoiding the
/// stock 1 MiB SkyrimSE stack that overflows under VFS hooks (`0xC00000FD`).
///
/// Unlike a bare RSP pivot onto an empty region (which broke Steam DRM /
/// RtlUserThreadStart), this:
/// 1. reads the live TEB StackBase + current RSP,
/// 2. copies `[RSP, StackBase)` onto the high end of a new reservation,
/// 3. adjusts RSP by the same delta, then updates TEB stack fields.
pub unsafe fn expand_primary_stack(
    process: HANDLE,
    primary: HANDLE,
    stack_reserve: usize,
) -> Result<(), InjectError> {
    type NtQueryInformationThreadFn =
        unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;

    let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
    if ntdll.is_null() {
        return Err(InjectError::Ntdll);
    }
    let nt_qit: NtQueryInformationThreadFn =
        match GetProcAddress(ntdll, c"NtQueryInformationThread".as_ptr().cast()) {
            Some(p) => core::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                NtQueryInformationThreadFn,
            >(p),
            None => return Err(InjectError::Ntdll),
        };

    // THREADINFOCLASS ThreadBasicInformation = 0
    // struct { ExitStatus, TebBaseAddress, ClientId, Affinity, Priority, BasePriority }
    #[repr(C)]
    struct ThreadBasicInformation {
        exit_status: i32,
        teb_base: u64,
        client_id: [u64; 2],
        affinity: u64,
        priority: i32,
        base_priority: i32,
    }
    let mut tbi: ThreadBasicInformation = zeroed();
    let mut ret_len = 0u32;
    let st = nt_qit(
        primary,
        0,
        &mut tbi as *mut _ as *mut c_void,
        size_of::<ThreadBasicInformation>() as u32,
        &mut ret_len,
    );
    if st != 0 || tbi.teb_base == 0 {
        eprintln!("vfs-inject: NtQueryInformationThread TEB failed status={st:x}");
        return Err(InjectError::CreateProcess);
    }

    let teb = tbi.teb_base as *mut c_void;
    // TEB x64: StackBase=0x08, StackLimit=0x10, DeallocationStack=0x1478
    let mut old_base = 0u64;
    let mut old_limit = 0u64;
    let mut old_dealloc = 0u64;
    let mut read_n = 0usize;
    for (off, dst) in [
        (0x08u64, &mut old_base),
        (0x10u64, &mut old_limit),
        (0x1478u64, &mut old_dealloc),
    ] {
        let ok = windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
            process,
            (teb as u64 + off) as *const c_void,
            dst as *mut u64 as *mut c_void,
            8,
            &mut read_n,
        );
        if ok == 0 || read_n != 8 || *dst == 0 {
            eprintln!("vfs-inject: TEB stack field read failed off={off:#x}");
            return Err(InjectError::CreateProcess);
        }
    }

    let mut ctx_buf = vec![0u8; size_of::<CONTEXT>() + 16];
    let ctx_addr = (ctx_buf.as_mut_ptr() as usize + 15) & !15;
    let ctx = ctx_addr as *mut CONTEXT;
    core::ptr::write_bytes(ctx as *mut u8, 0, size_of::<CONTEXT>());
    (*ctx).ContextFlags = CONTEXT_FULL;
    if GetThreadContext(primary, ctx) == 0 {
        return Err(InjectError::CreateProcess);
    }
    let old_rsp = (*ctx).Rsp;
    if old_rsp == 0 || old_rsp >= old_base || old_rsp < old_limit {
        eprintln!(
            "vfs-inject: primary RSP={old_rsp:#x} outside stack [{old_limit:#x},{old_base:#x})"
        );
        return Err(InjectError::CreateProcess);
    }
    let used = (old_base - old_rsp) as usize;
    // Leave headroom: used frames + 256 KiB slack must fit in the new reserve.
    if used + 256 * 1024 > stack_reserve {
        eprintln!("vfs-inject: live stack used={used:#x} exceeds expand target");
        return Err(InjectError::Alloc);
    }

    let stack = VirtualAllocEx(
        process,
        core::ptr::null(),
        stack_reserve,
        MEM_COMMIT | MEM_RESERVE,
        PAGE_READWRITE,
    );
    if stack.is_null() {
        return Err(InjectError::Alloc);
    }
    let new_base = stack as u64 + stack_reserve as u64; // high address (grows down)
                                                        // Guard-ish low page as StackLimit (committed but not used for frames).
    let new_limit = stack as u64 + 0x1000;
    let new_rsp = new_base - used as u64;

    // Copy live frames [old_rsp, old_base) → [new_rsp, new_base).
    let mut live = vec![0u8; used];
    let ok = windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
        process,
        old_rsp as *const c_void,
        live.as_mut_ptr() as *mut c_void,
        used,
        &mut read_n,
    );
    if ok == 0 || read_n != used {
        eprintln!("vfs-inject: read live stack frames failed used={used:#x}");
        return Err(InjectError::CreateProcess);
    }
    let mut written = 0usize;
    let ok = WriteProcessMemory(
        process,
        new_rsp as *mut c_void,
        live.as_ptr() as *const c_void,
        used,
        &mut written,
    );
    if ok == 0 || written != used {
        eprintln!("vfs-inject: write live stack frames failed");
        return Err(InjectError::Write);
    }

    for (off, val) in [
        (0x08u64, new_base),
        (0x10u64, new_limit),
        (0x1478u64, stack as u64),
    ] {
        let ok = WriteProcessMemory(
            process,
            (teb as u64 + off) as *mut c_void,
            &val as *const u64 as *const c_void,
            8,
            &mut written,
        );
        if ok == 0 || written != 8 {
            eprintln!("vfs-inject: TEB stack field write failed off={off:#x}");
            return Err(InjectError::Write);
        }
    }

    (*ctx).Rsp = new_rsp;
    if SetThreadContext(primary, ctx) == 0 {
        return Err(InjectError::CreateProcess);
    }
    eprintln!(
        "vfs-inject: expanded primary stack to {stack_reserve:#x} \
         (TEB={:#x} RSP {old_rsp:#x}→{new_rsp:#x} used={used:#x}; \
         old dealloc={old_dealloc:#x})",
        tbi.teb_base
    );
    Ok(())
}

/// `LoadLibraryW(dll_path)` in `process` on a remote thread, and wait up to
/// `timeout_ms` for it to return.
///
/// In a suspended process that has not run, the remote thread first runs the
/// process's initialisation (every static import and its `DllMain`), then
/// loads the shim, whose `DllMain` bootstraps before `LoadLibrary` returns. So
/// when this returns `Ok`, the shim has finished, one way or the other: its
/// ready file or ready event says which. The primary thread is not touched.
///
/// The remote thread gets [`PRIMARY_STACK_BYTES`]: process initialisation and
/// the shim's bootstrap run on it with the hooks going live.
///
/// # Safety
/// `process` must be a live process handle with `PROCESS_CREATE_THREAD`,
/// `PROCESS_VM_OPERATION` and `PROCESS_VM_WRITE`. This writes into and runs
/// code in another process; no part of that is checkable by the type system.
pub unsafe fn inject_dll(
    process: HANDLE,
    dll_path: &str,
    timeout_ms: u32,
) -> Result<(), InjectError> {
    // SAFETY: standard remote LoadLibrary injection; `process` is a live process
    // handle with the needed rights (per the contract above).
    unsafe {
        let dll_w = wide(dll_path);
        let bytes = dll_w.len() * 2;
        let remote = VirtualAllocEx(
            process,
            core::ptr::null(),
            bytes,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote.is_null() {
            return Err(InjectError::Alloc);
        }
        let mut written = 0usize;
        let ok = WriteProcessMemory(
            process,
            remote,
            dll_w.as_ptr() as *const c_void,
            bytes,
            &mut written,
        );
        if ok == 0 || written != bytes {
            return Err(InjectError::Write);
        }
        let k32 = GetModuleHandleW(wide("kernel32.dll").as_ptr());
        if k32.is_null() {
            return Err(InjectError::RemoteThread);
        }
        let load_library = match GetProcAddress(k32, c"LoadLibraryW".as_ptr().cast()) {
            Some(p) => p,
            None => return Err(InjectError::RemoteThread),
        };
        let start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            unsafe extern "system" fn(*mut c_void) -> u32,
        >(load_library));
        let hthread = CreateRemoteThread(
            process,
            core::ptr::null(),
            PRIMARY_STACK_BYTES,
            start,
            remote,
            STACK_SIZE_PARAM_IS_A_RESERVATION,
            core::ptr::null_mut(),
        );
        if hthread.is_null() || hthread == INVALID_HANDLE_VALUE {
            let err = windows_sys::Win32::Foundation::GetLastError();
            eprintln!("vfs-inject: CreateRemoteThread(LoadLibrary) failed last_error={err}");
            return Err(InjectError::RemoteThread);
        }
        let done = WaitForSingleObject(hthread, timeout_ms) == 0;
        CloseHandle(hthread);
        // The thread's exit code is only the low 32 bits of the HMODULE, so it
        // cannot tell a load from a failure; the shim's own signal does.
        if done {
            Ok(())
        } else {
            Err(InjectError::Timeout)
        }
    }
}

/// What the contents of the shim's ready file say.
#[derive(Debug)]
enum ReadyState {
    /// `READY_OK`: release the process.
    Ready,
    /// A failure spelling: the process must be killed with this error.
    Failed(InjectError),
    /// Empty, partial or unrecognised: keep waiting (a write in flight reads
    /// as this too). Never a reason to release.
    Pending,
}

fn classify_ready(content: &str) -> ReadyState {
    if content == vfs_env::READY_OK {
        ReadyState::Ready
    } else if let Some(msg) = content.strip_prefix(vfs_env::READY_FUSE_FAILED_PREFIX) {
        // Also how an older shim spelled a config error; its message says so.
        ReadyState::Failed(InjectError::FuseInit(msg.to_string()))
    } else if let Some(msg) = content.strip_prefix(vfs_env::READY_BOOTSTRAP_FAILED_PREFIX) {
        ReadyState::Failed(InjectError::Bootstrap(msg.to_string()))
    } else {
        ReadyState::Pending
    }
}

/// The one way a launch gives up on a target that has not been released: kill
/// it, close both handles, and hand back the error to return.
///
/// Every failure between `CreateProcess` and resuming the target ends here,
/// because the only alternative (resuming it, or leaving it parked) is a game
/// running without the shim, or a leaked process.
/// Cases where the target has already exited only close the handles.
///
/// # Safety
/// `pi` must hold the live handles from this launch's `CreateProcessW`, not
/// yet closed.
unsafe fn fail_closed(pi: &PROCESS_INFORMATION, e: InjectError) -> InjectError {
    // SAFETY: per the contract above.
    unsafe {
        let _ = TerminateProcess(pi.hProcess, 1);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
    }
    e
}

/// Launch the target with the shim.
///
/// A target exe that imports the shim first (staging patched it) is started
/// normally ([`run_import_activated`]). Any other is injected, the
/// way SKSE injects its DLL: created suspended, its primary stack grown to
/// [`PRIMARY_STACK_BYTES`], the shim `LoadLibrary`'d on a remote thread (which
/// runs process initialisation and then the shim's bootstrap), and resumed
/// only once the shim's ready file says it is up. Any failure kills it before
/// it has run.
pub fn run_target_with_shim(cfg: RunConfig) -> Result<i32, InjectError> {
    std::env::set_var(vfs_env::SHIM_CONFIG, &cfg.config_path);
    std::env::set_var(vfs_env::SHIM_READY, &cfg.ready_path);
    // Where the shim says which children it killed; descendants inherit it.
    let refused_log = format!("{}{}", cfg.ready_path, vfs_env::CHILD_REFUSED_SUFFIX);
    let _ = std::fs::remove_file(&refused_log);
    std::env::set_var(vfs_env::CHILD_REFUSED_LOG, &refused_log);
    // The shim waits for each child process it injects as long as this launch
    // waits for the shim: it reads the value back from the inherited environment.
    std::env::set_var(
        vfs_env::READY_TIMEOUT_SECS,
        cfg.ready_timeout.as_secs().max(1).to_string(),
    );
    // The managed root, so the child's fuse client matches the session root —
    // unless the caller already named it. The Proton launch sets
    // `VFS_VIRTUAL_DIR` to root 0 and a working directory that may be below
    // it; every Windows caller passes its root as `current_dir` and sets the
    // same value (or none), so for them nothing changes.
    if let Some(ref d) = cfg.current_dir {
        if std::env::var_os(vfs_env::VIRTUAL_DIR).is_none() {
            std::env::set_var(vfs_env::VIRTUAL_DIR, d);
        }
    }
    let _ = std::fs::remove_file(&cfg.ready_path);

    if crate::exe_imports_shim(&cfg.target_exe) {
        return run_import_activated(&cfg);
    }

    let timeout_ms = u32::try_from(cfg.ready_timeout.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: CreateProcessW + inject + resume.
    unsafe {
        let mut pi: PROCESS_INFORMATION = zeroed();
        // Staging puts the EXE and its import closure on disk, so the loader
        // resolves everything itself — see `vfs-director::stage`.
        let mut cmdline = format!("\"{}\"", cfg.target_exe);
        for a in &cfg.args {
            cmdline.push_str(&format!(" \"{a}\""));
        }
        let app_w = wide(&cfg.target_exe);
        let mut cmd_w = wide(&cmdline);
        let cwd_w = cfg.current_dir.as_ref().map(|s| wide(s));
        let mut si: STARTUPINFOW = zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let ok = CreateProcessW(
            app_w.as_ptr(),
            cmd_w.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            0,
            CREATE_SUSPENDED,
            core::ptr::null(),
            cwd_w
                .as_ref()
                .map(|v| v.as_ptr())
                .unwrap_or(core::ptr::null()),
            &si,
            &mut pi,
        );
        if ok == 0 {
            return Err(InjectError::CreateProcess);
        }

        // Best-effort — a failure resumes with the stock stack rather than
        // refusing to launch.
        if let Err(e) = expand_primary_stack(pi.hProcess, pi.hThread, PRIMARY_STACK_BYTES) {
            eprintln!(
                "vfs-inject: expand_primary_stack failed ({e:?}) — resuming with stock 1MiB stack"
            );
        }

        if let Err(e) = inject_dll(pi.hProcess, &cfg.dll_path, timeout_ms) {
            if let Some(code) = exited(pi.hProcess) {
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                return Err(InjectError::TargetExited(code));
            }
            return Err(fail_closed(&pi, e));
        }

        // The shim wrote its ready file before its `LoadLibrary` returned. Its
        // *content*, not merely its existence, is the protocol (see
        // `vfs_env::READY_OK`, `READY_FUSE_FAILED_PREFIX`,
        // `READY_BOOTSTRAP_FAILED_PREFIX`). Anything but "ready" — a failure
        // spelling, a shim that never loaded, a target that died in process
        // initialisation — kills the target, which has not run.
        let state = std::fs::read_to_string(&cfg.ready_path)
            .map(|c| classify_ready(&c))
            .unwrap_or(ReadyState::Pending);
        match state {
            ReadyState::Ready => {}
            ReadyState::Failed(e) => return Err(fail_closed(&pi, e)),
            ReadyState::Pending => {
                if let Some(code) = exited(pi.hProcess) {
                    CloseHandle(pi.hThread);
                    CloseHandle(pi.hProcess);
                    return Err(InjectError::TargetExited(code));
                }
                return Err(fail_closed(
                    &pi,
                    InjectError::Bootstrap("the shim loaded but never wrote its ready file".into()),
                ));
            }
        }

        ResumeThread(pi.hThread);

        if cfg.detach {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Ok(0);
        }

        if WaitForSingleObject(pi.hProcess, INFINITE) != 0 {
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Err(InjectError::Wait);
        }
        let mut code: u32 = 0;
        let got = GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        if got == 0 {
            return Err(InjectError::ExitCode);
        }
        Ok(code as i32)
    }
}

/// Start a target whose exe imports the shim first. No suspend, no
/// payload, no remote thread: the loader runs the shim's `DllMain` before any
/// other import initialises, and a failed bootstrap fails process start there.
/// This only starts the process and reports how it ended.
fn run_import_activated(cfg: &RunConfig) -> Result<i32, InjectError> {
    let mut cmdline = format!("\"{}\"", cfg.target_exe);
    for a in &cfg.args {
        cmdline.push_str(&format!(" \"{a}\""));
    }
    let app_w = wide(&cfg.target_exe);
    let mut cmd_w = wide(&cmdline);
    let cwd_w = cfg.current_dir.as_ref().map(|s| wide(s));
    let started = Instant::now();
    // SAFETY: CreateProcessW with valid, NUL-terminated buffers; handles closed below.
    unsafe {
        let mut pi: PROCESS_INFORMATION = zeroed();
        let mut si: STARTUPINFOW = zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let ok = CreateProcessW(
            app_w.as_ptr(),
            cmd_w.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            0,
            0,
            core::ptr::null(),
            cwd_w
                .as_ref()
                .map(|v| v.as_ptr())
                .unwrap_or(core::ptr::null()),
            &si,
            &mut pi,
        );
        if ok == 0 {
            return Err(InjectError::CreateProcess);
        }
        CloseHandle(pi.hThread);
        // Report when the shim said ready, for the timing comparison.
        loop {
            if let Ok(c) = std::fs::read_to_string(&cfg.ready_path) {
                if !c.is_empty() {
                    eprintln!(
                        "vfs-inject: import activation: shim ready after {} ms: {c}",
                        started.elapsed().as_millis()
                    );
                    break;
                }
            }
            if exited(pi.hProcess).is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if cfg.detach {
            CloseHandle(pi.hProcess);
            return Ok(0);
        }
        WaitForSingleObject(pi.hProcess, INFINITE);
        let code = exited(pi.hProcess);
        CloseHandle(pi.hProcess);
        match code {
            // The loader refused to start it: the shim was missing or its
            // DllMain failed. Nothing of the program ran.
            Some(c @ (0xC000_0135 | 0xC000_0142)) => Err(InjectError::TargetExited(c)),
            Some(c) => Ok(c as i32),
            None => Err(InjectError::ExitCode),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_file_contents_map_to_release_kill_or_wait() {
        assert!(matches!(classify_ready("ready"), ReadyState::Ready));
        assert!(matches!(
            classify_ready("fuse-failed:no ring"),
            ReadyState::Failed(InjectError::FuseInit(m)) if m == "no ring"
        ));
        assert!(matches!(
            classify_ready("bootstrap-failed:shim config version 3"),
            ReadyState::Failed(InjectError::Bootstrap(m)) if m == "shim config version 3"
        ));
        // Nothing here may ever read as Ready: a half-written file keeps waiting.
        for partial in ["", "rea", "ready ", "boot", "bootstrap", "Ready"] {
            assert!(
                matches!(classify_ready(partial), ReadyState::Pending),
                "{partial:?}"
            );
        }
    }
}
