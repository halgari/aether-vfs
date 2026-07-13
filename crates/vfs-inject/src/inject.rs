//! All Win32 injection FFI. Validated by the dll-injection spike.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, CreateRemoteThread, GetExitCodeProcess, ResumeThread, WaitForSingleObject,
    CREATE_SUSPENDED, INFINITE, LPTHREAD_START_ROUTINE, PROCESS_INFORMATION, STARTUPINFOW,
};

use crate::{InjectError, RunConfig};

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

/// Inject `dll_path` into a process via `LoadLibraryW` on a remote thread.
fn inject_dll(process: HANDLE, dll_path: &str) -> Result<(), InjectError> {
    // SAFETY: standard remote LoadLibrary injection; `process` is a live process
    // handle with the needed rights (from CreateProcessW). Validated by spike.
    unsafe {
        let dll_w = wide(dll_path);
        let bytes = dll_w.len() * 2;
        let remote = VirtualAllocEx(process, core::ptr::null(), bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if remote.is_null() {
            return Err(InjectError::Alloc);
        }
        let mut written = 0usize;
        let ok = WriteProcessMemory(process, remote, dll_w.as_ptr() as *const c_void, bytes, &mut written);
        if ok == 0 || written != bytes {
            return Err(InjectError::Write);
        }
        let k32 = GetModuleHandleW(wide("kernel32.dll").as_ptr());
        if k32.is_null() {
            return Err(InjectError::RemoteThread);
        }
        let load_library = match GetProcAddress(k32, b"LoadLibraryW\0".as_ptr()) {
            Some(p) => p,
            None => return Err(InjectError::RemoteThread),
        };
        let start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute(load_library));
        let hthread = CreateRemoteThread(process, core::ptr::null(), 0, start, remote, 0, core::ptr::null_mut());
        if hthread.is_null() || hthread == INVALID_HANDLE_VALUE {
            return Err(InjectError::RemoteThread);
        }
        WaitForSingleObject(hthread, INFINITE);
        CloseHandle(hthread);
        Ok(())
    }
}

/// Launch the target suspended, inject the shim, wait for readiness, resume, and
/// return the target's exit code.
pub fn run_target_with_shim(cfg: RunConfig) -> Result<i32, InjectError> {
    // The child inherits our env (null lpEnvironment), so set the shim vars here.
    std::env::set_var("VFS_SHIM_CONFIG", &cfg.config_path);
    std::env::set_var("VFS_SHIM_READY", &cfg.ready_path);
    let _ = std::fs::remove_file(&cfg.ready_path);

    // Build the command line: "exe" "arg1" "arg2" ... (mutable buffer required).
    let mut cmdline = format!("\"{}\"", cfg.target_exe);
    for a in &cfg.args {
        cmdline.push_str(&format!(" \"{a}\""));
    }
    let app_w = wide(&cfg.target_exe);
    let mut cmd_w = wide(&cmdline);

    // SAFETY: standard CreateProcessW + inject + resume; handles are closed on
    // every exit path. Validated by spike.
    unsafe {
        let mut si: STARTUPINFOW = zeroed();
        si.cb = size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = zeroed();
        let ok = CreateProcessW(
            app_w.as_ptr(),
            cmd_w.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            0,
            CREATE_SUSPENDED,
            core::ptr::null(),
            core::ptr::null(),
            &si,
            &mut pi,
        );
        if ok == 0 {
            return Err(InjectError::CreateProcess);
        }

        // Inject; on failure, tear the process down.
        if let Err(e) = inject_dll(pi.hProcess, &cfg.dll_path) {
            let _ = ResumeThread(pi.hThread); // let it die naturally
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Err(e);
        }

        // Wait for the shim to signal it installed the hook.
        let deadline = Instant::now() + cfg.ready_timeout;
        while !std::path::Path::new(&cfg.ready_path).exists() {
            if Instant::now() >= deadline {
                CloseHandle(pi.hThread);
                CloseHandle(pi.hProcess);
                return Err(InjectError::Timeout);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Resume and wait for exit.
        ResumeThread(pi.hThread);
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
