//! Child-process propagation: dual-layer inject (early payload + full shim)
//! into force-suspended children, readiness events, self-DLL path discovery.
#![allow(unsafe_code)]

use core::ffi::c_void;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::{ReadProcessMemory, WriteProcessMemory};
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW, GetProcAddress,
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateRemoteThread, GetCurrentProcessId, ResumeThread, SetEvent,
    SuspendThread, WaitForSingleObject, LPTHREAD_START_ROUTINE,
};

use vfs_inject::{arm_preinit_payload_ex, PreinitRedirect};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Per-PID file the child bootstrap reads for early Config address (hex).
/// Parent writes this after arming; avoids relying on inherited env addresses.
pub fn payload_cfg_path_for_pid(pid: u32) -> PathBuf {
    std::env::temp_dir().join(format!("vfs_payload_cfg_{pid}.txt"))
}

/// The readiness event name for a given process id.
fn ready_event_name(pid: u32) -> Vec<u16> {
    wide(&format!(r"Local\vfs_shim_ready_{pid}"))
}

/// Absolute path of `vfs_payload.dll` for dual-layer child inject.
/// Prefers `VFS_PAYLOAD_PATH`, then co-locates/copies beside this shim DLL
/// (searches parent / deps / current exe).
pub fn payload_dll_path() -> Option<String> {
    let self_dll = self_dll_path()?;
    let preferred = std::env::var("VFS_PAYLOAD_PATH").ok();
    vfs_inject::ensure_payload_beside_shim(&self_dll, preferred.as_deref())
}

/// Early redirect table for children: same static-import list as the parent,
/// loaded from `VFS_SHIM_CONFIG` (inherited when `lpEnvironment` is null).
fn child_preinit_redirects() -> Vec<PreinitRedirect> {
    const MAX: usize = 4;
    let path = match std::env::var("VFS_SHIM_CONFIG") {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };
    // Prefer vfs-inject parser (same wire format) so child matches director.
    vfs_inject::merge_preinit_redirects(&path, &[])
        .into_iter()
        .take(MAX)
        .collect()
}

/// Inject `dll_path` into `process` via `LoadLibraryW` on a remote thread and
/// wait for that thread (i.e. for `DllMain` to run).
pub fn inject_dll(process: HANDLE, dll_path: &str) -> bool {
    // SAFETY: standard remote-LoadLibrary injection into a live child process.
    unsafe {
        let dll_w = wide(dll_path);
        let bytes = dll_w.len() * 2;
        let remote =
            VirtualAllocEx(process, core::ptr::null(), bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if remote.is_null() {
            return false;
        }
        let mut written = 0usize;
        let ok = WriteProcessMemory(process, remote, dll_w.as_ptr() as *const c_void, bytes, &mut written);
        if ok == 0 || written != bytes {
            return false;
        }
        let k32 = GetModuleHandleW(wide("kernel32.dll").as_ptr());
        if k32.is_null() {
            return false;
        }
        let load = match GetProcAddress(k32, b"LoadLibraryW\0".as_ptr()) {
            Some(p) => p,
            None => return false,
        };
        let start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute(load));
        let th =
            CreateRemoteThread(process, core::ptr::null(), 0, start, remote, 0, core::ptr::null_mut());
        if th.is_null() || th == INVALID_HANDLE_VALUE {
            return false;
        }
        WaitForSingleObject(th, INFINITE_MS);
        CloseHandle(th);
        true
    }
}

/// Dual-layer inject into a force-suspended child (same vehicle as the director):
/// arm early payload with spin gate → resume → wait install sentinel →
/// LoadLibrary full shim → wait ready → release spin.
///
/// On any failure after the primary may be spinning, attempts to release the
/// gate. Returns whether dual-layer completed successfully.
pub fn inject_child_dual_layer(
    process: HANDLE,
    thread: HANDLE,
    pid: u32,
    full_shim_dll: &str,
    timeout_ms: u32,
) -> bool {
    let payload = match payload_dll_path() {
        Some(p) => p,
        None => return false,
    };
    let redirects = child_preinit_redirects();
    // SAFETY: `process`/`thread` come from our own `CreateProcessInternalW`
    // hook, which forced CREATE_SUSPENDED, so the child is live and suspended
    // and we hold the rights the call needs.
    let arm = match unsafe { arm_preinit_payload_ex(process, thread, &payload, &redirects, true) } {
        Ok(a) => a,
        Err(_) => return false,
    };

    // Child bootstrap finds cfg via PID file (env may still hold parent's path).
    let cfg_path = payload_cfg_path_for_pid(pid);
    if std::fs::write(&cfg_path, format!("{:x}", arm.cfg_remote)).is_err() {
        release_spin(process, arm.release_flag);
        return false;
    }

    // SAFETY: thread from CreateProcess force-suspend; resume to run stub.
    unsafe {
        if ResumeThread(thread) == u32::MAX {
            release_spin(process, arm.release_flag);
            return false;
        }
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    // Wait for early install sentinel (counters[7] == 0xC0DE).
    loop {
        if read_u32(process, arm.counters + 0x1C) == Some(0xC0DE) {
            break;
        }
        if Instant::now() >= deadline {
            release_spin(process, arm.release_flag);
            let _ = std::fs::remove_file(&cfg_path);
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    if !inject_dll(process, full_shim_dll) {
        release_spin(process, arm.release_flag);
        let _ = std::fs::remove_file(&cfg_path);
        return false;
    }

    if !wait_ready(pid, timeout_ms) {
        release_spin(process, arm.release_flag);
        let _ = std::fs::remove_file(&cfg_path);
        return false;
    }

    if !release_spin(process, arm.release_flag) {
        let _ = std::fs::remove_file(&cfg_path);
        return false;
    }

    // Best-effort cleanup of the one-shot cfg file (child already read it).
    let _ = std::fs::remove_file(&cfg_path);
    true
}

/// Inject into a suspended child: dual-layer if payload is available, else
/// classic LoadLibrary-only. Then wait for readiness.
pub fn inject_child(
    process: HANDLE,
    thread: HANDLE,
    pid: u32,
    full_shim_dll: &str,
    timeout_ms: u32,
) -> bool {
    if inject_child_dual_layer(process, thread, pid, full_shim_dll, timeout_ms) {
        return true;
    }
    // Fallback: classic remote LoadLibrary (no static-import pre-init).
    if inject_dll(process, full_shim_dll) {
        wait_ready(pid, timeout_ms)
    } else {
        false
    }
}

fn release_spin(process: HANDLE, release_flag: u64) -> bool {
    if release_flag == 0 {
        return true;
    }
    let one = 1u32.to_le_bytes();
    // SAFETY: release_flag is in the child's address space (from arm).
    unsafe {
        let mut n = 0usize;
        WriteProcessMemory(
            process,
            release_flag as *const c_void,
            one.as_ptr() as *const c_void,
            4,
            &mut n,
        ) != 0
            && n == 4
    }
}

fn read_u32(process: HANDLE, addr: u64) -> Option<u32> {
    let mut buf = [0u8; 4];
    let mut n = 0usize;
    // SAFETY: best-effort RPM of a known remote diagnostics word.
    unsafe {
        let ok = ReadProcessMemory(
            process,
            addr as *const c_void,
            buf.as_mut_ptr() as *mut c_void,
            4,
            &mut n,
        );
        if ok != 0 && n == 4 {
            Some(u32::from_le_bytes(buf))
        } else {
            None
        }
    }
}

/// The absolute path of the DLL this code lives in.
pub fn self_dll_path() -> Option<String> {
    // SAFETY: resolve our module by an address inside it, then read its path.
    unsafe {
        let mut hmod = core::ptr::null_mut();
        let ok = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            inject_dll as *const u16,
            &mut hmod,
        );
        if ok == 0 || hmod.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 32768];
        let n = GetModuleFileNameW(hmod, buf.as_mut_ptr(), buf.len() as u32);
        if n == 0 || n as usize >= buf.len() {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..n as usize]))
    }
}

/// Signal that the current process's shim has installed its hooks.
pub fn signal_ready() {
    // SAFETY: named-event create + set; the leaked handle is process-lifetime.
    unsafe {
        let name = ready_event_name(GetCurrentProcessId());
        let ev = CreateEventW(core::ptr::null(), 1, 0, name.as_ptr());
        if !ev.is_null() {
            SetEvent(ev);
        }
    }
}

/// Wait up to `timeout_ms` for `pid`'s shim to signal readiness.
pub fn wait_ready(pid: u32, timeout_ms: u32) -> bool {
    // SAFETY: named-event create + timed wait; handle closed before return.
    unsafe {
        let name = ready_event_name(pid);
        let ev = CreateEventW(core::ptr::null(), 1, 0, name.as_ptr());
        if ev.is_null() {
            return false;
        }
        let r = WaitForSingleObject(ev, timeout_ms);
        CloseHandle(ev);
        r == 0
    }
}

/// Re-suspend a child the caller originally asked to keep suspended.
pub fn re_suspend(thread: HANDLE) {
    // SAFETY: thread handle from CreateProcess; best-effort.
    unsafe {
        let _ = SuspendThread(thread);
    }
}

const INFINITE_MS: u32 = 0xFFFF_FFFF;
