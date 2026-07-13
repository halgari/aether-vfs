//! Injectable shim DLL. On load, a background thread bootstraps the shim from the
//! config file named by `VFS_SHIM_CONFIG` and signals readiness via
//! `VFS_SHIM_READY`. Kept minimal — real work happens off the loader lock.
#![allow(unsafe_code)]

use core::ffi::c_void;
use windows_sys::Win32::Foundation::{BOOL, HINSTANCE, TRUE};

const DLL_PROCESS_ATTACH: u32 = 1;

/// Standard DLL entry point. Spawns a thread (loader lock forbids heavy work
/// here) that installs the hook, then returns immediately.
#[no_mangle]
pub extern "system" fn DllMain(_dll: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        std::thread::spawn(bootstrap);
    }
    TRUE
}

/// Runs on a fresh thread after `DllMain` returns. Bootstraps the shim and, on
/// success, leaks the guard (hook persists for the process lifetime) and writes
/// the ready marker. On failure, signals nothing — the director times out.
fn bootstrap() {
    let config = match std::env::var("VFS_SHIM_CONFIG") {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Ok(guard) = vfs_shim::bootstrap_from_config_path(&config) {
        core::mem::forget(guard);
        if let Ok(ready) = std::env::var("VFS_SHIM_READY") {
            let _ = std::fs::write(&ready, b"ready");
        }
    }
}
