//! Injectable shim DLL.
//!
//! - Classic path: `DllMain` spawns a thread that bootstraps from
//!   `VFS_SHIM_CONFIG` and signals `VFS_SHIM_READY`.
//! - Dual-layer path (`VFS_DUAL_LAYER` set): `DllMain` does not spawn; the
//!   OEP late-entry stub calls [`vfs_shim_sync_bootstrap`] synchronously after
//!   LoadLibrary so hooks are live before EXE main.
#![allow(unsafe_code)]

use core::ffi::c_void;
use windows_sys::Win32::Foundation::{BOOL, HINSTANCE, TRUE};

const DLL_PROCESS_ATTACH: u32 = 1;

/// Standard DLL entry point. Always spawns bootstrap off the loader lock.
/// Dual-layer uses `VFS_PAYLOAD_CFG_FILE` so bootstrap can `install_late`.
#[no_mangle]
pub extern "system" fn DllMain(_dll: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        std::thread::spawn(bootstrap);
    }
    TRUE
}

/// Synchronous bootstrap for dual-layer OEP late-entry.
/// `payload_cfg` is the early payload Config address (or null for full install).
#[no_mangle]
pub extern "system" fn vfs_shim_sync_bootstrap(payload_cfg: *mut c_void) -> u32 {
    vfs_shim::sync_bootstrap(payload_cfg)
}

/// Classic async bootstrap (loader-lock safe: runs off DllMain).
fn bootstrap() {
    let config = match std::env::var("VFS_SHIM_CONFIG") {
        Ok(c) => c,
        Err(_) => {
            log_boot("VFS_SHIM_CONFIG unset");
            return;
        }
    };
    match vfs_shim::bootstrap_from_config_path(&config) {
        Ok(guard) => {
            core::mem::forget(guard);
            if let Ok(ready) = std::env::var("VFS_SHIM_READY") {
                let _ = std::fs::write(&ready, b"ready");
            }
        }
        Err(e) => {
            log_boot(&format!("bootstrap_from_config_path({config}) failed: {e:?}"));
        }
    }
}

fn log_boot(msg: &str) {
    if let Ok(ready) = std::env::var("VFS_SHIM_READY") {
        let path = format!("{ready}.boot.log");
        let _ = std::fs::write(&path, msg.as_bytes());
    }
    // Also try a fixed temp path so we always have a breadcrumb.
    let _ = std::fs::write(
        std::env::temp_dir().join("vfs_shim_boot.log"),
        msg.as_bytes(),
    );
}
