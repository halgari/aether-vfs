//! Win32 launch and injection: start the target, or start it suspended and
//! `LoadLibrary` the shim into it on a remote thread.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
// the primary stack's expansion (x86-64 only)
#[cfg(not(target_arch = "x86"))]
use windows_sys::Win32::System::Diagnostics::Debug::{GetThreadContext, SetThreadContext, CONTEXT};

#[cfg(not(target_arch = "x86"))]
const CONTEXT_FULL: u32 = 0x0010_000B;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CreateRemoteThread, GetExitCodeProcess, ResumeThread, TerminateProcess,
    WaitForSingleObject, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, INFINITE,
    LPTHREAD_START_ROUTINE, PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::tree::{finish_code, Job, ProcessTree};
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
///
/// x86-64 only (the x64 TEB offsets and `Rsp`): a 32-bit target keeps its stock
/// stack, as any target does when this fails, which the caller logs.
#[cfg(target_arch = "x86")]
pub unsafe fn expand_primary_stack(
    _process: HANDLE,
    _primary: HANDLE,
    _stack_reserve: usize,
) -> Result<(), InjectError> {
    Err(InjectError::ThreadContext)
}

#[cfg(not(target_arch = "x86"))]
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
    // SAFETY: as this function's contract.
    unsafe { inject_dll_exit_code(process, dll_path, timeout_ms) }.map(|_| ())
}

/// [`inject_dll`], returning the remote `LoadLibraryW` thread's exit code: the
/// low 32 bits of the shim's `HMODULE`, so `0` means the load failed, and an
/// `NTSTATUS` (`0xC…`) means the process's initialisation, which that thread
/// ran first, failed. For a diagnosis; whether the shim came up is its own
/// signal's to say.
///
/// # Safety
/// As [`inject_dll`].
pub unsafe fn inject_dll_exit_code(
    process: HANDLE,
    dll_path: &str,
    timeout_ms: u32,
) -> Result<u32, InjectError> {
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
        // The thread's exit code is only the low 32 bits of the HMODULE, so it
        // cannot tell a load from a failure for certain; the shim's own signal
        // does. It is still the best clue when that signal never comes.
        let mut code = 0u32;
        windows_sys::Win32::System::Threading::GetExitCodeThread(hthread, &mut code);
        CloseHandle(hthread);
        if done {
            Ok(code)
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

/// Launch the target with the shim, and wait for it (unless `cfg.detach`).
///
/// A target exe that imports the shim first (staging patched it) is started
/// normally ([`start_import_activated`]). Any other is injected, the
/// way SKSE injects its DLL: created suspended, its primary stack grown to
/// [`PRIMARY_STACK_BYTES`], the shim `LoadLibrary`'d on a remote thread (which
/// runs process initialisation and then the shim's bootstrap), and resumed
/// only once the shim's ready file says it is up. Any failure kills it before
/// it has run.
///
/// **Waits for the first process only.** This is what `vfs-injector.exe` runs
/// inside Wine, where the Proton launch watches the whole prefix instead; a
/// host that needs the whole process tree uses [`spawn_target_with_shim`].
pub fn run_target_with_shim(cfg: RunConfig) -> Result<i32, InjectError> {
    let env = launch_env_block(&cfg);
    let started = start_target(&cfg, None, &env)?;
    // SAFETY: `started.process` is the live handle `start_target` handed over;
    // it is closed on every path below.
    unsafe {
        if cfg.detach {
            CloseHandle(started.process);
            return Ok(0);
        }
        if WaitForSingleObject(started.process, INFINITE) != 0 {
            CloseHandle(started.process);
            return Err(InjectError::Wait);
        }
        let code = exited(started.process);
        CloseHandle(started.process);
        match code {
            Some(c) => finish_code(started.import_activated, c),
            None => Err(InjectError::ExitCode),
        }
    }
}

/// Launch the target with the shim, exactly as [`run_target_with_shim`]
/// does, inside a Job Object, and return once it is running.
///
/// The target is assigned to the job while it is still suspended, before any
/// of its code (or the shim's) has run, so everything it starts inherits the
/// job: the returned [`ProcessTree`] ends when the last of them exits, and
/// reports the first process's exit code. `cfg.detach` is ignored.
///
/// A target that cannot be put in a job (the host is in one that forbids
/// nesting) is launched anyway and tracked alone: see
/// [`ProcessTree::untracked_reason`].
pub fn spawn_target_with_shim(cfg: RunConfig) -> Result<ProcessTree, InjectError> {
    let env = launch_env_block(&cfg);
    let (mut job, no_job) = match Job::new() {
        Ok(j) => (Some(j), None),
        Err(e) => (
            None,
            Some(format!(
                "no job for the launch ({e}), so it ends when the launched process exits, \
                 not when everything it started has"
            )),
        ),
    };
    let started = start_target(&cfg, job.as_mut(), &env)?;
    // SAFETY: `started.process` is live and owned; the tree takes it over.
    Ok(unsafe {
        ProcessTree::new(
            started.process,
            started.pid,
            started.import_activated,
            job,
            no_job,
        )
    })
}

/// The environment block the target starts with, and a clean slate for its
/// ready file. This process's own environment is read, never written: see
/// `env_block`.
fn launch_env_block(cfg: &RunConfig) -> Vec<u16> {
    // Where the shim says which children it killed; descendants inherit it.
    let refused_log = format!("{}{}", cfg.ready_path, vfs_env::CHILD_REFUSED_SUFFIX);
    let _ = std::fs::remove_file(&refused_log);
    let _ = std::fs::remove_file(&cfg.ready_path);
    let mut set: Vec<(String, String)> = vec![
        (vfs_env::SHIM_CONFIG.into(), cfg.config_path.clone()),
        (vfs_env::SHIM_READY.into(), cfg.ready_path.clone()),
        (vfs_env::CHILD_REFUSED_LOG.into(), refused_log),
        // The shim waits for each child process it injects as long as this
        // launch waits for the shim: it reads the value back from its
        // environment.
        (
            vfs_env::READY_TIMEOUT_SECS.into(),
            cfg.ready_timeout.as_secs().max(1).to_string(),
        ),
    ];
    // The managed root, so the child's fuse client matches the session root —
    // unless the caller or this process already names it. The Proton launch
    // sets `VFS_VIRTUAL_DIR` to root 0 and a working directory that may be
    // below it.
    if let Some(ref d) = cfg.current_dir {
        let named = |n: &str| n.eq_ignore_ascii_case(vfs_env::VIRTUAL_DIR);
        let removed = cfg.env_remove.iter().any(|n| named(n));
        if !cfg.env_set.iter().any(|(n, _)| named(n))
            && (removed || std::env::var_os(vfs_env::VIRTUAL_DIR).is_none())
        {
            set.push((vfs_env::VIRTUAL_DIR.into(), d.clone()));
        }
    }
    set.extend(cfg.env_set.iter().cloned());
    crate::env_block::build(&crate::env_block::current(), &set, &cfg.env_remove)
}

/// A target that is running with its shim up.
struct Started {
    /// Its process handle, owned by the receiver. The thread handle is closed.
    process: HANDLE,
    pid: u32,
    /// It imported the shim itself — see [`finish_code`].
    import_activated: bool,
}

/// Start the target with the shim (see [`run_target_with_shim`]) and, when
/// `job` is given, assign it to the job before it runs.
fn start_target(
    cfg: &RunConfig,
    job: Option<&mut Job>,
    env: &[u16],
) -> Result<Started, InjectError> {
    if crate::exe_imports_shim(&cfg.target_exe) {
        return start_import_activated(cfg, job, env);
    }

    let timeout_ms = u32::try_from(cfg.ready_timeout.as_millis()).unwrap_or(u32::MAX - 1);
    // SAFETY: CreateProcessW + inject + resume.
    unsafe {
        let mut pi: PROCESS_INFORMATION = zeroed();
        // Staging puts the EXE and its import closure on disk, so the loader
        // resolves everything itself — see `vfs-director::stage`.
        let mut cmd_w = wide(&command_line(cfg));
        let app_w = wide(&cfg.target_exe);
        let cwd_w = cfg.current_dir.as_ref().map(|s| wide(s));
        let mut si: STARTUPINFOW = zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let ok = CreateProcessW(
            app_w.as_ptr(),
            cmd_w.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            0,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
            env.as_ptr().cast(),
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

        // Before anything of the target runs — the remote thread below runs
        // its process initialisation — so nothing it starts escapes the job.
        if let Some(job) = job {
            job.adopt(pi.hProcess);
        }

        // Before process initialisation reads them: kernelbase keeps its own
        // copy of the command line from then on.
        if let Err(e) = tell_where(pi.hProcess, cfg) {
            return Err(fail_closed(&pi, e));
        }

        // Best-effort — a failure resumes with the stock stack rather than
        // refusing to launch.
        if let Err(e) = expand_primary_stack(pi.hProcess, pi.hThread, PRIMARY_STACK_BYTES) {
            eprintln!(
                "vfs-inject: expand_primary_stack failed ({e:?}) — resuming with stock 1MiB stack"
            );
        }

        // Prefer the in-memory import patch: the loader then loads the shim
        // first, so its `DllMain` runs before the target's own static imports
        // initialise. Remote-thread injection runs those imports first (they
        // read the game's files, which are in the VFS, before the shim is up),
        // which is what refused `SkyrimSE.exe`. Fall back to it only when the
        // in-memory patch is refused, and say which path was taken.
        if !cfg.force_injection {
            match crate::patch_imports_in_memory(pi.hProcess) {
                Ok(()) => {
                    eprintln!("vfs-inject: activation: in-memory import patch");
                    ResumeThread(pi.hThread);
                    // The shim bootstraps as the loader processes imports, after
                    // the resume, and writes the ready file. Poll it to the
                    // deadline, as the import-activated path does.
                    let state = poll_ready(pi.hProcess, &cfg.ready_path, cfg.ready_timeout);
                    if let Err(e) = state {
                        return Err(fail_closed(&pi, e));
                    }
                    CloseHandle(pi.hThread);
                    return Ok(Started {
                        process: pi.hProcess,
                        pid: pi.dwProcessId,
                        import_activated: false,
                    });
                }
                Err(e) => eprintln!(
                "vfs-inject: activation: remote injection (in-memory import patch refused: {e:?})"
            ),
            }
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
        CloseHandle(pi.hThread);
        Ok(Started {
            process: pi.hProcess,
            pid: pi.dwProcessId,
            import_activated: false,
        })
    }
}

/// Poll `ready_path` until the shim writes its verdict, the target exits, or
/// `timeout` passes. For the in-memory import patch, where the shim runs only
/// after the primary thread is resumed.
///
/// # Safety
/// `process` is the live, resumed target.
#[allow(unsafe_code)]
unsafe fn poll_ready(
    process: HANDLE,
    ready_path: &str,
    timeout: Duration,
) -> Result<(), InjectError> {
    let began = Instant::now();
    loop {
        match std::fs::read_to_string(ready_path)
            .map(|c| classify_ready(&c))
            .unwrap_or(ReadyState::Pending)
        {
            ReadyState::Ready => return Ok(()),
            ReadyState::Failed(e) => return Err(e),
            ReadyState::Pending => {}
        }
        // SAFETY: a zero-timeout poll of a handle we own.
        if let Some(code) = exited(process) {
            return Err(InjectError::TargetExited(code));
        }
        if began.elapsed() > timeout {
            return Err(InjectError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The target's command line: its path and each argument, quoted.
fn command_line(cfg: &RunConfig) -> String {
    command_line_for(&cfg.target_exe, cfg)
}

/// [`command_line`] with `image` as the first argument.
fn command_line_for(image: &str, cfg: &RunConfig) -> String {
    let mut cmdline = format!("\"{image}\"");
    for a in &cfg.args {
        cmdline.push_str(&format!(" \"{a}\""));
    }
    cmdline
}

/// Tell the suspended target its virtual image path and working directory,
/// if it has them (see `procparams`).
///
/// # Safety
/// `process` is the suspended target's `CreateProcessW` handle.
unsafe fn tell_where(process: HANDLE, cfg: &RunConfig) -> Result<(), InjectError> {
    let cmd = cfg
        .virtual_image
        .as_deref()
        .map(|v| command_line_for(v, cfg));
    // SAFETY: as this function's contract.
    unsafe { crate::procparams::rewrite(process, cmd.as_deref(), cfg.virtual_cwd.as_deref()) }
        .map_err(InjectError::Bootstrap)
}

/// Start a target whose exe imports the shim first. No payload, no remote
/// thread: the loader runs the shim's `DllMain` before any other import
/// initialises, and a failed bootstrap fails process start there. Returns
/// once the shim has said something in its ready file, or the target has
/// exited.
///
/// Created suspended only to be assigned to `job` before it runs, then
/// resumed at once; without a job it is started running, as it always was.
fn start_import_activated(
    cfg: &RunConfig,
    job: Option<&mut Job>,
    env: &[u16],
) -> Result<Started, InjectError> {
    let mut cmd_w = wide(&command_line(cfg));
    let app_w = wide(&cfg.target_exe);
    let cwd_w = cfg.current_dir.as_ref().map(|s| wide(s));
    let started = Instant::now();
    // Suspended to be put in `job`, or to be told where it is, before it runs.
    let rewrite = cfg.virtual_image.is_some() || cfg.virtual_cwd.is_some();
    let flags = if job.is_some() || rewrite {
        CREATE_SUSPENDED
    } else {
        0
    };
    // SAFETY: CreateProcessW with valid, NUL-terminated buffers; the thread
    // handle is closed here and the process handle handed to the caller.
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
            flags | CREATE_UNICODE_ENVIRONMENT,
            env.as_ptr().cast(),
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
        if let Some(job) = job {
            job.adopt(pi.hProcess);
        }
        if flags & CREATE_SUSPENDED != 0 {
            if let Err(e) = tell_where(pi.hProcess, cfg) {
                return Err(fail_closed(&pi, e));
            }
            ResumeThread(pi.hThread);
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
        Ok(Started {
            process: pi.hProcess,
            pid: pi.dwProcessId,
            import_activated: true,
        })
    }
}

/// Resume a suspended thread (the child's primary, after an in-memory import
/// patch). A no-op on a thread that is already running.
///
/// # Safety
/// `thread` is a live thread handle with `THREAD_SUSPEND_RESUME`.
#[allow(unsafe_code)]
pub unsafe fn resume_primary_thread(thread: HANDLE) {
    // SAFETY: a resume of a thread handle we own.
    unsafe {
        ResumeThread(thread);
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
