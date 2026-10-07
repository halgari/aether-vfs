//! The shim DLL.
//!
//! `DllMain` bootstraps synchronously on `DLL_PROCESS_ATTACH`, however the DLL
//! arrived: as the first static import of a patched exe (import activation),
//! or through a remote `LoadLibrary` into a suspended process (injection). In
//! both cases nothing of the program has run yet, and a failed bootstrap
//! returns `FALSE`, which fails the load: process start (0xC0000142) for an
//! import, the `LoadLibrary` for an injection, whose launcher then kills the
//! process. Either way the program never runs un-virtualised.
#![allow(unsafe_code)]

use core::ffi::c_void;
use windows_sys::Win32::Foundation::{HINSTANCE, TRUE};

const DLL_PROCESS_ATTACH: u32 = 1;

/// Standard DLL entry point.
/// (windows-sys 0.61 dropped the `BOOL` alias; the ABI return is a plain `i32`.)
///
/// Deliberately does **nothing** on `DLL_PROCESS_DETACH`. Flushing a final
/// hook-stats report from there was built and measured on 2026-08-15, and it
/// wedged injected processes at exit — every other thread is already
/// terminated by then, and one killed mid-write leaves a lock the flush waits
/// on forever inside the loader lock. See `vfs_shim::hookstats::banner` for
/// the measurement and for what the reports say instead.
///
/// Bootstrap runs under the loader lock. It must not wait on another thread or
/// load a library whose `DllMain` would: it reads its config, maps the
/// director's ring and patches ntdll, and the threads it starts are not waited
/// for.
///
/// ## Panic containment
///
/// This is an `extern "system"` entry point, so an unwind out of it is an
/// immediate `abort()` — inside the loader lock, the worst place in the process
/// to die. `vfs_shim::contain_panic` is the same wrapper the ntdll detours use.
/// A panic fails the load like any other bootstrap failure, and leaves a
/// breadcrumb.
#[no_mangle]
pub extern "system" fn DllMain(_dll: HINSTANCE, reason: u32, _reserved: *mut c_void) -> i32 {
    vfs_shim::contain_panic(
        "DllMain",
        || {
            if reason != DLL_PROCESS_ATTACH {
                return TRUE;
            }
            let ok = bootstrap();
            vfs_shim::finish_ready_handshake();
            if ok {
                TRUE
            } else {
                // A spawning parent waits on this rather than on its timeout.
                vfs_shim::signal_bootstrap_failed();
                0
            }
        },
        || {
            log_boot("DllMain panicked — this process is NOT virtualized");
            vfs_shim::signal_bootstrap_failed();
            0
        },
    )
}

/// The export a patched exe imports, so the loader has a symbol to bind. It
/// does nothing: the work happened in `DllMain`, before anything could call it.
#[no_mangle]
pub extern "system" fn vfs_shim_activated() -> u32 {
    vfs_shim::contain_panic("vfs_shim_activated", || 1, || 0)
}

/// Whether the shim came up. A failure is also written to the ready file, so a
/// launcher polling it reports why.
fn bootstrap() -> bool {
    let config = match vfs_env::text(vfs_env::SHIM_CONFIG).ok_or(()) {
        Ok(c) => c,
        Err(_) => {
            log_boot("VFS_SHIM_CONFIG unset");
            return false;
        }
    };
    match vfs_shim::bootstrap_from_config_path(&config) {
        Ok(guard) => {
            core::mem::forget(guard);
            if let Some(ready) = vfs_env::text(vfs_env::SHIM_READY) {
                let _ = std::fs::write(&ready, vfs_env::READY_OK);
            }
            true
        }
        // A director was configured and FUSE failed to attach.
        Err(vfs_shim::BootstrapError::Fuse(msg)) => {
            log_boot(&format!("FUSE init failed: {msg}"));
            if let Some(ready) = vfs_env::text(vfs_env::SHIM_READY) {
                let _ = std::fs::write(
                    &ready,
                    format!("{}{msg}", vfs_env::READY_FUSE_FAILED_PREFIX),
                );
            }
            false
        }
        // Any other bootstrap failure (a config from another build, an unreadable
        // config, a hook that would not install).
        Err(e) => {
            log_boot(&format!(
                "bootstrap_from_config_path({config}) failed: {e:?}"
            ));
            if let Some(ready) = vfs_env::text(vfs_env::SHIM_READY) {
                let _ = std::fs::write(&ready, vfs_shim::bootstrap_failed_content(&e));
            }
            false
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
