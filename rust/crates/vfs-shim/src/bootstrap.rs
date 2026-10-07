//! Bootstrap glue: the config-file entry point used by the injected DLL to attach
//! the director's client and install the hooks. The config codec is
//! `vfs_protocol::shimcfg`.
#![allow(unsafe_code)] // payload_cfg_usable VirtualQuery validation

use core::ffi::c_void;

use crate::hook::{HookGuard, InstallError, install, install_late};
use vfs_inject::PayloadConfig;
use vfs_protocol::shimcfg::{self, ConfigError, StaticImport};

/// True when `p` looks like a live early-payload Config in *this* process
/// (readable page + nt_protect matches our ntdll). Rejects inherited parent
/// addresses in child processes.
fn payload_cfg_usable(p: *mut PayloadConfig) -> bool {
    if p.is_null() {
        return false;
    }
    // SAFETY: VirtualQuery on an arbitrary address is defined; we only read
    // through p after the query says the page is committed.
    unsafe {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery,
        };
        let mut mbi = core::mem::MaybeUninit::<MEMORY_BASIC_INFORMATION>::uninit();
        let n = VirtualQuery(
            p as *const c_void,
            mbi.as_mut_ptr(),
            core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        );
        if n == 0 {
            return false;
        }
        let mbi = mbi.assume_init();
        if mbi.State != MEM_COMMIT {
            return false;
        }
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        if ntdll.is_null() {
            return false;
        }
        let expected = match GetProcAddress(ntdll, c"NtProtectVirtualMemory".as_ptr().cast()) {
            Some(f) => f as usize,
            None => return false,
        };
        (*p).nt_protect == expected
    }
}

/// Errors bootstrapping the shim from a config file.
#[derive(Debug)]
pub enum BootstrapError {
    /// The config file could not be read.
    Io,
    /// The config did not decode: truncated or malformed, or written by a host from a
    /// different build (no version header, or another version). The message names both
    /// versions; a launcher shows it, because the fix is rebuilding the pair together.
    Config(ConfigError),
    /// A director was configured (a ring was named) but the FUSE client
    /// failed to attach. Fails before any hook is installed, and the game's
    /// primary thread stays parked behind
    /// the pre-init spin gate — nothing has run yet, so the caller can (and
    /// must) tear the process down rather than let it start un-virtualised.
    Fuse(String),
    /// The hook could not be installed.
    Install(InstallError),
}

/// The ready-file content that reports a bootstrap failure that is not the
/// director's: [`vfs_env::READY_BOOTSTRAP_FAILED_PREFIX`] and the reason. A
/// config error carries its own message (it names both versions).
pub fn bootstrap_failed_content(e: &BootstrapError) -> String {
    let why = match e {
        BootstrapError::Config(c) => c.to_string(),
        other => format!("{other:?}"),
    };
    format!("{}{why}", vfs_env::READY_BOOTSTRAP_FAILED_PREFIX)
}

/// Read a config file, attach the director's client, and install the hooks. Returns the
/// guard keeping the hooks alive (the injected DLL leaks it).
///
/// When `payload_cfg` is non-null, uses dual-layer [`install_late`] (early
/// payload already owns the four path/attr stubs). Otherwise full [`install`].
pub fn bootstrap_from_config_path(path: &str) -> Result<HookGuard, BootstrapError> {
    bootstrap_from_config_path_with_payload(path, core::ptr::null_mut())
}

/// Like [`bootstrap_from_config_path`], with an optional early-payload Config
/// pointer for dual-layer secondary publish.
pub fn bootstrap_from_config_path_with_payload(
    path: &str,
    payload_cfg: *mut PayloadConfig,
) -> Result<HookGuard, BootstrapError> {
    let bytes = std::fs::read(path).map_err(|_| BootstrapError::Io)?;
    // The root is the director's to answer for (the client's roots come from the environment), so
    // decoding is only the version and shape check: a config from another build fails here, loudly.
    shimcfg::decode_config(&bytes).map_err(BootstrapError::Config)?;
    // Attach to the parent director's FUSE ring. Standalone (no-director)
    // shim launches are retired: a process that names no ring at all
    // (`NotConfigured`) used to be treated as a legitimate deployment, with
    // the shim composing the tree on its own — but that is
    // exactly the mode in which a game runs completely un-virtualised while
    // appearing to work, which this whole programme exists to eliminate. It
    // fails exactly like a named ring that failed to attach (`ConnectFailed`):
    // before any hook installs. The client's roots (`director::roots_from_env`)
    // are the only notion of "under a root" the hooks have.
    match crate::director::try_init_from_env() {
        Ok(()) => {}
        Err(crate::director::FuseInitError::NotConfigured) => {
            return Err(BootstrapError::Fuse(
                "no VFS_RING_SECTION configured: standalone (no-director) shim launches are \
                 retired — a director must be attached"
                    .to_string(),
            ));
        }
        Err(crate::director::FuseInitError::ConnectFailed(msg)) => {
            return Err(BootstrapError::Fuse(msg));
        }
    }

    // Dual-layer cfg sources (first usable wins):
    // 1. explicit pointer (sync bootstrap / tests)
    // 2. VFS_PAYLOAD_CFG_FILE env (director dual-layer)
    // 3. per-PID temp file written by parent CPIW (child dual-layer)
    // Validate before install_late — inherited parent paths/addresses must not
    // be trusted (wrong process VA space).
    let cfg_ptr = {
        let from_arg = if !payload_cfg.is_null() && payload_cfg_usable(payload_cfg) {
            payload_cfg
        } else {
            core::ptr::null_mut()
        };
        if !from_arg.is_null() {
            from_arg
        } else {
            let mut candidates: Vec<std::path::PathBuf> = Vec::new();
            if let Some(file) = vfs_env::text(vfs_env::PAYLOAD_CFG_FILE) {
                candidates.push(std::path::PathBuf::from(file));
            }
            candidates.push(crate::child::payload_cfg_path_for_pid(std::process::id()));
            candidates
                .into_iter()
                .find_map(|file| {
                    std::fs::read_to_string(&file)
                        .ok()
                        .and_then(|s| usize::from_str_radix(s.trim(), 16).ok())
                        .map(|a| a as *mut PayloadConfig)
                        .filter(|p| payload_cfg_usable(*p))
                })
                .unwrap_or(core::ptr::null_mut())
        }
    };

    let guard = if cfg_ptr.is_null() {
        install().map_err(BootstrapError::Install)?
    } else {
        // SAFETY: `cfg_ptr` was parsed from the address the injector published
        // for this process, and is non-null on this branch.
        unsafe { install_late(cfg_ptr).map_err(BootstrapError::Install)? }
    };
    // Tell any spawning parent (that force-suspended us) our hooks are live.
    crate::child::signal_ready();
    Ok(guard)
}

/// Synchronous dual-layer bootstrap entry used by the OEP late-entry stub
/// after `LoadLibrary` of this DLL. Returns 0 on success, non-zero on failure.
///
/// `payload_cfg` must be null or a valid early-payload Config in this process
/// (caller contract; not checked).
pub fn sync_bootstrap(payload_cfg: *mut c_void) -> u32 {
    let Some(config) = vfs_env::text(vfs_env::SHIM_CONFIG) else {
        return 1;
    };
    let cfg = payload_cfg as *mut PayloadConfig;
    match bootstrap_from_config_path_with_payload(&config, cfg) {
        Ok(guard) => {
            core::mem::forget(guard);
            if let Some(ready) = vfs_env::text(vfs_env::SHIM_READY) {
                let _ = std::fs::write(&ready, vfs_env::READY_OK);
            }
            0
        }
        // A director was configured and the FUSE client failed to attach.
        // Still write the ready file — the caller (`run_target_with_shim`)
        // is spin-waiting on it — but with the failure spelling instead of
        // "ready", so it terminates this (still fully parked, pre-release)
        // process instead of letting it run un-virtualised.
        Err(BootstrapError::Fuse(msg)) => {
            if let Some(ready) = vfs_env::text(vfs_env::SHIM_READY) {
                let _ = std::fs::write(
                    &ready,
                    format!("{}{msg}", vfs_env::READY_FUSE_FAILED_PREFIX),
                );
            }
            3
        }
        // A config from another build or a damaged one, an unreadable config, a hook that
        // would not install: say so in the ready file, so the launcher kills the parked
        // process and reports why instead of waiting out its timeout.
        Err(e) => {
            if let Some(ready) = vfs_env::text(vfs_env::SHIM_READY) {
                let _ = std::fs::write(&ready, bootstrap_failed_content(&e));
            }
            2
        }
    }
}

/// Load static-import entries from a config file on disk. `None` when the file cannot be read or
/// does not decode (bootstrap reports that; this reader only serves the early-payload rows).
pub fn load_static_imports_from_config_path(path: &str) -> Option<Vec<StaticImport>> {
    let bytes = std::fs::read(path).ok()?;
    Some(shimcfg::decode_config(&bytes).ok()?.static_imports)
}

/// Convert config static imports into early-payload redirect rows (NT paths +
/// sizes). Skips missing backing files. Caps at `max` entries (payload limit).
pub fn static_imports_to_preinit(
    statics: &[StaticImport],
    max: usize,
) -> Vec<(String, String, u64)> {
    let mut out = Vec::new();
    for e in statics.iter().take(max) {
        let path = e.backing_path.trim();
        if path.is_empty() || e.dll_name.trim().is_empty() {
            continue;
        }
        // Strip NT prefix for std::fs::metadata if present.
        let win_path = path.strip_prefix(r"\??\").unwrap_or(path);
        let size = match std::fs::metadata(win_path) {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        let backing_nt = if path.starts_with(r"\??\") {
            path.to_string()
        } else {
            format!(r"\??\{path}")
        };
        // Suffix = final component of dll_name (tolerate "d3d11.dll" or paths).
        let suffix = e
            .dll_name
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(e.dll_name.as_str())
            .to_string();
        out.push((suffix, backing_nt, size));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Use `matches!` on the whole Result rather than `.unwrap_err()`: the latter
    // needs the Ok type `HookGuard: Debug`, which it deliberately is not (it owns
    // a RawDetour; a guard type carries no useful Debug). Same convention the rest
    // of the workspace uses for resource/guard types.
    #[test]
    fn bootstrap_missing_file_is_io_error() {
        assert!(matches!(
            bootstrap_from_config_path(r"C:\nope\does-not-exist.cfg"),
            Err(BootstrapError::Io)
        ));
    }

    // Direct unit coverage for `payload_cfg_usable`'s garbage-pointer rejection —
    // the safety property `bad_payload_cfg.rs` used to exercise indirectly via a
    // full `bootstrap_from_config_path` call. That integration test was removed
    // for gate 3, Task 3 ("retire standalone mode"): with no `VFS_RING_SECTION`
    // configured, `bootstrap_from_config_path` now aborts on
    // `FuseInitError::NotConfigured` before ever reaching the dual-layer
    // cfg-pointer logic these two functions guard, so it could no longer reach
    // the code it was meant to test. Standing up a real director ring just to
    // reach a null/garbage-pointer guard several steps past the FUSE gate would
    // be disproportionate to what the guard itself asserts, so this narrower,
    // ring-free unit test takes its place instead of leaving the guard
    // unverified.
    #[test]
    fn payload_cfg_usable_rejects_null() {
        assert!(!payload_cfg_usable(core::ptr::null_mut()));
    }

    #[test]
    fn payload_cfg_usable_rejects_unmapped_garbage_address() {
        // A dangling (never-committed) pointer: VirtualQuery must report it as
        // such (MEM_FREE), so this must be rejected before ever dereferencing it.
        assert!(!payload_cfg_usable(
            std::ptr::dangling_mut::<PayloadConfig>()
        ));
    }

    #[test]
    fn bootstrap_garbage_config_is_bad_config() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("vfs-shim-badcfg-{}.bin", std::process::id()));
        std::fs::write(&path, [0u8, 1]).unwrap(); // too short for the header
        assert!(matches!(
            bootstrap_from_config_path(path.to_str().unwrap()),
            Err(BootstrapError::Config(_))
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// A config from another build is refused at bootstrap, before the ring is touched, with an
    /// error that names the versions.
    #[test]
    fn bootstrap_refuses_a_config_from_another_build() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("vfs-shim-oldcfg-{}.bin", std::process::id()));
        // A version-1 config: root, overlay, "VFS1", n_static, snapshot.
        let mut v1 = Vec::new();
        v1.extend_from_slice(&1u32.to_le_bytes());
        v1.push(b'R');
        v1.extend_from_slice(&0u32.to_le_bytes());
        v1.extend_from_slice(b"VFS1");
        v1.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(&path, &v1).unwrap();
        let r = bootstrap_from_config_path(path.to_str().unwrap());
        assert!(matches!(r, Err(BootstrapError::Config(ConfigError::Unversioned))));
        let mut v3 = shimcfg::encode_config("R");
        v3[4..8].copy_from_slice(&3u32.to_le_bytes());
        std::fs::write(&path, &v3).unwrap();
        let r = bootstrap_from_config_path(path.to_str().unwrap());
        assert!(matches!(
            r,
            Err(BootstrapError::Config(ConfigError::Version { found: 3, .. }))
        ));
        let _ = std::fs::remove_file(&path);
    }

    /// What the launcher reads in the ready file for a config refusal: the bootstrap spelling
    /// (not the director's), carrying the message that names both versions.
    #[test]
    fn a_config_refusal_is_spelled_for_the_ready_file() {
        let e = BootstrapError::Config(ConfigError::Version { found: 3, expected: 2 });
        let content = bootstrap_failed_content(&e);
        assert!(content.starts_with(vfs_env::READY_BOOTSTRAP_FAILED_PREFIX), "{content}");
        assert!(content.contains("version 3") && content.contains("version 2"), "{content}");
        assert!(!content.starts_with(vfs_env::READY_FUSE_FAILED_PREFIX));
        let io = bootstrap_failed_content(&BootstrapError::Io);
        assert_eq!(io, format!("{}Io", vfs_env::READY_BOOTSTRAP_FAILED_PREFIX));
    }
}
