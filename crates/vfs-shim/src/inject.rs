//! Child-process propagation primitives: remote-thread DLL injection, locating
//! our own DLL on disk, and a per-PID readiness event so a spawning process can
//! wait for a child's shim to install its hooks before resuming it. All `unsafe`
//! FFI; validated by the child-process spike.
#![allow(unsafe_code)]

use core::ffi::c_void;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows_sys::Win32::System::LibraryLoader::{
    GetModuleFileNameW, GetModuleHandleExW, GetModuleHandleW, GetProcAddress,
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows_sys::Win32::System::Memory::{
    VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateRemoteThread, GetCurrentProcessId, SetEvent, WaitForSingleObject,
    LPTHREAD_START_ROUTINE,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The readiness event name for a given process id.
fn ready_event_name(pid: u32) -> Vec<u16> {
    wide(&format!(r"Local\vfs_shim_ready_{pid}"))
}

/// Inject `dll_path` into `process` via `LoadLibraryW` on a remote thread and
/// wait for that thread (i.e. for `DllMain` to run). Returns whether it
/// succeeded; a failure is non-fatal to the child (the caller resumes it
/// anyway, merely unvirtualized).
pub fn inject_dll(process: HANDLE, dll_path: &str) -> bool {
    // SAFETY: standard remote-LoadLibrary injection into a live child process
    // handle carrying the needed rights (from the forced-suspend create).
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

/// The absolute path of the DLL this code lives in (so it can inject the same
/// shim into children). `None` if the module handle or filename lookup fails.
pub fn self_dll_path() -> Option<String> {
    // SAFETY: resolve our module by an address inside it, then read its path.
    unsafe {
        let mut hmod = core::ptr::null_mut();
        let ok = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            inject_dll as *const u16, // an address within this module
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

/// Signal that the current process's shim has installed its hooks: set the
/// per-PID readiness event a spawning parent may be waiting on. The event is
/// created if absent (manual-reset) and intentionally leaked so it stays set.
pub fn signal_ready() {
    // SAFETY: named-event create + set; the leaked handle is process-lifetime.
    unsafe {
        let name = ready_event_name(GetCurrentProcessId());
        let ev = CreateEventW(core::ptr::null(), 1 /*manual reset*/, 0 /*not signaled*/, name.as_ptr());
        if !ev.is_null() {
            SetEvent(ev);
            // Leak: the event must outlive this call so a waiter still sees it.
        }
    }
}

/// Wait up to `timeout_ms` for `pid`'s shim to signal readiness. Returns whether
/// it signaled in time. Creating the (manual-reset) event here first guarantees
/// it exists before the child sets it, avoiding a lost-wakeup race.
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
        r == 0 // WAIT_OBJECT_0
    }
}

const INFINITE_MS: u32 = 0xFFFF_FFFF;
