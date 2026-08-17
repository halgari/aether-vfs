//! Injectable shim DLL.
//!
//! - Classic path: `DllMain` spawns a thread that bootstraps from
//!   `VFS_SHIM_CONFIG` and signals `VFS_SHIM_READY`.
//! - Dual-layer path (`VFS_DUAL_LAYER` set): `DllMain` does not spawn; the
//!   OEP late-entry stub calls [`vfs_shim_sync_bootstrap`] synchronously after
//!   LoadLibrary so hooks are live before EXE main.
#![allow(unsafe_code)]

use core::ffi::c_void;
use windows_sys::Win32::Foundation::{HINSTANCE, TRUE};

const DLL_PROCESS_ATTACH: u32 = 1;

/// Standard DLL entry point. Always spawns bootstrap off the loader lock.
/// Dual-layer uses `VFS_PAYLOAD_CFG_FILE` so bootstrap can `install_late`.
/// (windows-sys 0.61 dropped the `BOOL` alias; the ABI return is a plain `i32`.)
///
/// Deliberately does **nothing** on `DLL_PROCESS_DETACH`. Flushing a final
/// hook-stats report from there was built and measured on 2026-08-15, and it
/// wedged injected processes at exit — every other thread is already
/// terminated by then, and one killed mid-write leaves a lock the flush waits
/// on forever inside the loader lock. See `vfs_shim::hookstats::banner` for
/// the measurement and for what the reports say instead.
///
/// ## Panic containment
///
/// This is an `extern "system"` entry point, so an unwind out of it is an
/// immediate `abort()` of the game — inside the loader lock, which is the worst
/// place in the process to die. `vfs_shim::contain_panic` is the same wrapper all
/// twenty ntdll detours use.
///
/// **On a panic it returns `TRUE` and leaves a breadcrumb**, rather than `FALSE`.
/// `FALSE` fails the `LoadLibrary` and looks like "the shim did not load", which
/// is indistinguishable from a dozen ordinary causes. The only thing this
/// function does is spawn `bootstrap`, so a panic here means bootstrap never
/// started and the ready file is never written — which
/// `vfs_inject::run_target_with_shim` already handles as `InjectError::Timeout`,
/// after which it releases (classic path) or terminates the child. So the
/// existing handshake reports it; returning `TRUE` just avoids replacing a
/// diagnosable timeout with a loader failure.
#[no_mangle]
pub extern "system" fn DllMain(_dll: HINSTANCE, reason: u32, _reserved: *mut c_void) -> i32 {
    vfs_shim::contain_panic(
        "DllMain",
        || {
            if reason == DLL_PROCESS_ATTACH {
                std::thread::spawn(bootstrap);
            }
            TRUE
        },
        || {
            log_boot("DllMain panicked — bootstrap was not spawned, so this process is NOT virtualized");
            TRUE
        },
    )
}

/// Synchronous bootstrap for dual-layer OEP late-entry.
/// `payload_cfg` is the early payload Config address (or null for full install).
///
/// Returns 0 on success and non-zero on failure — `1` no config, `2` bootstrap
/// failed, `3` FUSE init failed, and [`SYNC_BOOTSTRAP_PANICKED`] for a contained
/// panic. The OEP stub reads this, so the panic has a value to report and does
/// not need to invent an encoding.
#[no_mangle]
pub extern "system" fn vfs_shim_sync_bootstrap(payload_cfg: *mut c_void) -> u32 {
    vfs_shim::contain_panic(
        "vfs_shim_sync_bootstrap",
        || vfs_shim::sync_bootstrap(payload_cfg),
        || {
            log_boot("vfs_shim_sync_bootstrap panicked — hooks are NOT installed");
            SYNC_BOOTSTRAP_PANICKED
        },
    )
}

/// What [`vfs_shim_sync_bootstrap`] returns when its body panicked.
///
/// A distinct code rather than reusing `2` ("bootstrap failed"): the two want
/// different responses. `2` is a configured failure the shim understood; this is
/// a bug, and the hook-panic counters plus the boot log are where it is recorded.
/// Non-zero is what matters to the caller either way — it must never be 0, which
/// would tell the stub that hooks are live when nothing is installed.
pub const SYNC_BOOTSTRAP_PANICKED: u32 = 4;

/// Classic async bootstrap (loader-lock safe: runs off DllMain).
fn bootstrap() {
    let config = match vfs_env::text(vfs_env::SHIM_CONFIG).ok_or(()) {
        Ok(c) => c,
        Err(_) => {
            log_boot("VFS_SHIM_CONFIG unset");
            return;
        }
    };
    match vfs_shim::bootstrap_from_config_path(&config) {
        Ok(guard) => {
            core::mem::forget(guard);
            if let Some(ready) = vfs_env::text(vfs_env::SHIM_READY) {
                let _ = std::fs::write(&ready, vfs_env::READY_OK);
            }
        }
        // A director was configured and FUSE failed to attach — same
        // failure-spelling protocol as the dual-layer `sync_bootstrap` path,
        // so any launcher polling the ready file sees the same signal
        // regardless of which bootstrap path ran.
        Err(vfs_shim::BootstrapError::Fuse(msg)) => {
            log_boot(&format!("FUSE init failed: {msg}"));
            if let Some(ready) = vfs_env::text(vfs_env::SHIM_READY) {
                let _ = std::fs::write(&ready, format!("{}{msg}", vfs_env::READY_FUSE_FAILED_PREFIX));
            }
        }
        Err(e) => {
            log_boot(&format!("bootstrap_from_config_path({config}) failed: {e:?}"));
        }
    }
}

fn log_boot(msg: &str) {
    if let Some(ready) = vfs_env::text(vfs_env::SHIM_READY) {
        let path = format!("{ready}.boot.log");
        let _ = std::fs::write(&path, msg.as_bytes());
    }
    // Also try a fixed temp path so we always have a breadcrumb.
    let _ = std::fs::write(
        std::env::temp_dir().join("vfs_shim_boot.log"),
        msg.as_bytes(),
    );
}
