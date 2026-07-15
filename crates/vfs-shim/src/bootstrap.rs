//! Bootstrap glue: a tiny config codec and a config-file entry point used by the
//! injected DLL to build an `Engine` and install the hook.
#![allow(unsafe_code)] // payload_cfg_usable VirtualQuery validation

use core::ffi::c_void;

use crate::engine::{Engine, EngineError};
use crate::hook::{install, install_late, HookGuard, InstallError};
use crate::payload_abi::PayloadConfig;

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
            VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT,
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
        let ntdll = GetModuleHandleA(b"ntdll.dll\0".as_ptr());
        if ntdll.is_null() {
            return false;
        }
        let expected = match GetProcAddress(ntdll, b"NtProtectVirtualMemory\0".as_ptr()) {
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
    /// The config bytes were malformed.
    BadConfig,
    /// The engine could not be built (bad root or snapshot).
    Engine(EngineError),
    /// The hook could not be installed.
    Install(InstallError),
}

/// Read a config file, build an `Engine`, and install the hooks. Returns the
/// guard keeping the hooks alive (the injected DLL leaks it). An empty
/// `overlay_root` means read-only (no write overlay).
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
    let (root, overlay, snapshot) = decode_config(&bytes).ok_or(BootstrapError::BadConfig)?;
    // Attach to parent director FUSE ring when env/config is present.
    let _ = crate::fuse_client::try_init_from_env();
    let engine = if overlay.is_empty() {
        Engine::new(&root, snapshot)
    } else {
        Engine::with_overlay(&root, &overlay, snapshot)
    }
    .map_err(BootstrapError::Engine)?;

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
            if let Ok(file) = std::env::var("VFS_PAYLOAD_CFG_FILE") {
                candidates.push(std::path::PathBuf::from(file));
            }
            candidates.push(crate::inject::payload_cfg_path_for_pid(std::process::id()));
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
        install(engine).map_err(BootstrapError::Install)?
    } else {
        install_late(engine, cfg_ptr).map_err(BootstrapError::Install)?
    };
    // Tell any spawning parent (that force-suspended us) our hooks are live.
    crate::inject::signal_ready();
    Ok(guard)
}

/// Synchronous dual-layer bootstrap entry used by the OEP late-entry stub
/// after `LoadLibrary` of this DLL. Returns 0 on success, non-zero on failure.
///
/// `payload_cfg` must be null or a valid early-payload Config in this process
/// (caller contract; not checked).
pub fn sync_bootstrap(payload_cfg: *mut c_void) -> u32 {
    let config = match std::env::var("VFS_SHIM_CONFIG") {
        Ok(c) => c,
        Err(_) => return 1,
    };
    let cfg = payload_cfg as *mut PayloadConfig;
    match bootstrap_from_config_path_with_payload(&config, cfg) {
        Ok(guard) => {
            core::mem::forget(guard);
            if let Ok(ready) = std::env::var("VFS_SHIM_READY") {
                let _ = std::fs::write(&ready, b"ready");
            }
            0
        }
        Err(_) => 2,
    }
}

/// One static-import DLL virtualization: the EXE's import of `dll_name`
/// (final path component, e.g. `d3d11.dll`) is redirected pre-init to
/// `backing_path` (absolute Win32 path; NT `\??\` form also accepted).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticImport {
    pub dll_name: String,
    pub backing_path: String,
}

/// Magic after root+overlay marking the extended config section (static imports).
/// Legacy configs omit this and treat the remainder as the snapshot blob.
const CONFIG_MAGIC: &[u8; 4] = b"VFS1";

/// Encode with no write overlay and no static imports.
pub fn encode_config(root: &str, snapshot: &[u8]) -> Vec<u8> {
    encode_config_full(root, "", &[], snapshot)
}

/// Encode with overlay, no static imports.
pub fn encode_config_with_overlay(root: &str, overlay: &str, snapshot: &[u8]) -> Vec<u8> {
    encode_config_full(root, overlay, &[], snapshot)
}

/// Full config: root, overlay, static-import table, snapshot.
///
/// Wire format:
/// ```text
/// [u32 root_len][root utf8]
/// [u32 overlay_len][overlay utf8]
/// "VFS1"
/// [u32 n_static]
/// n times: [u32 name_len][name utf8][u32 backing_len][backing utf8]
/// [snapshot bytes…]
/// ```
///
/// Legacy files without the `VFS1` marker still decode (static list empty).
pub fn encode_config_full(
    root: &str,
    overlay: &str,
    static_imports: &[StaticImport],
    snapshot: &[u8],
) -> Vec<u8> {
    let root_b = root.as_bytes();
    let overlay_b = overlay.as_bytes();
    let mut out = Vec::with_capacity(16 + root_b.len() + overlay_b.len() + snapshot.len() + 64);
    out.extend_from_slice(&(root_b.len() as u32).to_le_bytes());
    out.extend_from_slice(root_b);
    out.extend_from_slice(&(overlay_b.len() as u32).to_le_bytes());
    out.extend_from_slice(overlay_b);
    out.extend_from_slice(CONFIG_MAGIC);
    out.extend_from_slice(&(static_imports.len() as u32).to_le_bytes());
    for e in static_imports {
        let n = e.dll_name.as_bytes();
        let b = e.backing_path.as_bytes();
        out.extend_from_slice(&(n.len() as u32).to_le_bytes());
        out.extend_from_slice(n);
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(b);
    }
    out.extend_from_slice(snapshot);
    out
}

/// Decode a config buffer. Returns `(root, overlay, static_imports, snapshot)`.
/// `None` on truncation or invalid UTF-8. Never panics.
pub fn decode_config_full(bytes: &[u8]) -> Option<(String, String, Vec<StaticImport>, Vec<u8>)> {
    let read_field = |b: &[u8], off: usize| -> Option<(String, usize)> {
        let len = u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?) as usize;
        let start = off + 4;
        let end = start.checked_add(len)?;
        let s = std::str::from_utf8(b.get(start..end)?).ok()?.to_string();
        Some((s, end))
    };
    let (root, after_root) = read_field(bytes, 0)?;
    let (overlay, after_overlay) = read_field(bytes, after_root)?;
    let rest = bytes.get(after_overlay..)?;
    if rest.len() >= 4 && &rest[..4] == CONFIG_MAGIC {
        let mut off = 4usize;
        let n = u32::from_le_bytes(rest.get(off..off + 4)?.try_into().ok()?) as usize;
        off += 4;
        let mut statics = Vec::with_capacity(n.min(16));
        for _ in 0..n {
            let (name, o1) = read_field(rest, off)?;
            let (backing, o2) = read_field(rest, o1)?;
            off = o2;
            statics.push(StaticImport {
                dll_name: name,
                backing_path: backing,
            });
        }
        let snapshot = rest.get(off..)?.to_vec();
        Some((root, overlay, statics, snapshot))
    } else {
        // Legacy: remainder is pure snapshot.
        Some((root, overlay, Vec::new(), rest.to_vec()))
    }
}

/// Decode legacy-compatible shape: `(root, overlay, snapshot)`. Static imports
/// discarded — use [`decode_config_full`] when you need them.
pub fn decode_config(bytes: &[u8]) -> Option<(String, String, Vec<u8>)> {
    let (root, overlay, _statics, snapshot) = decode_config_full(bytes)?;
    Some((root, overlay, snapshot))
}

/// Load static-import entries from a config file on disk.
pub fn load_static_imports_from_config_path(path: &str) -> Option<Vec<StaticImport>> {
    let bytes = std::fs::read(path).ok()?;
    let (_r, _o, statics, _s) = decode_config_full(&bytes)?;
    Some(statics)
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

    #[test]
    fn config_round_trips() {
        let snapshot = vec![1u8, 2, 3, 4, 5];
        let bytes = encode_config(r"\??\C:\Games\Skyrim", &snapshot);
        let (root, overlay, snap) = decode_config(&bytes).unwrap();
        assert_eq!(root, r"\??\C:\Games\Skyrim");
        assert_eq!(overlay, ""); // encode_config -> no overlay
        assert_eq!(snap, snapshot);
        let (_r, _o, statics, _s) = decode_config_full(&bytes).unwrap();
        assert!(statics.is_empty());
    }

    #[test]
    fn config_with_overlay_round_trips() {
        let snapshot = vec![9u8, 8, 7];
        let bytes = encode_config_with_overlay(r"C:\Game", r"C:\Overlay", &snapshot);
        let (root, overlay, snap) = decode_config(&bytes).unwrap();
        assert_eq!(root, r"C:\Game");
        assert_eq!(overlay, r"C:\Overlay");
        assert_eq!(snap, snapshot);
    }

    #[test]
    fn config_with_static_imports_round_trips() {
        let snapshot = vec![1u8, 2, 3];
        let statics = vec![
            StaticImport {
                dll_name: "d3d11.dll".into(),
                backing_path: r"C:\Mods\d3d11_proxy.dll".into(),
            },
            StaticImport {
                dll_name: "dxgi.dll".into(),
                backing_path: r"\??\C:\Mods\dxgi_proxy.dll".into(),
            },
        ];
        let bytes = encode_config_full(r"C:\Game", r"C:\Overlay", &statics, &snapshot);
        let (root, overlay, got, snap) = decode_config_full(&bytes).unwrap();
        assert_eq!(root, r"C:\Game");
        assert_eq!(overlay, r"C:\Overlay");
        assert_eq!(got, statics);
        assert_eq!(snap, snapshot);
        // Legacy decode still returns the snapshot correctly.
        let (_r, _o, snap2) = decode_config(&bytes).unwrap();
        assert_eq!(snap2, snapshot);
    }

    #[test]
    fn legacy_config_without_magic_still_decodes() {
        // Hand-build pre-VFS1 layout: root + overlay + raw snapshot.
        let mut bytes = Vec::new();
        let root = b"C:\\G";
        let overlay = b"";
        let snap = b"SNAP";
        bytes.extend_from_slice(&(root.len() as u32).to_le_bytes());
        bytes.extend_from_slice(root);
        bytes.extend_from_slice(&(overlay.len() as u32).to_le_bytes());
        bytes.extend_from_slice(overlay);
        bytes.extend_from_slice(snap);
        let (r, o, statics, s) = decode_config_full(&bytes).unwrap();
        assert_eq!(r, "C:\\G");
        assert_eq!(o, "");
        assert!(statics.is_empty());
        assert_eq!(s, snap);
    }

    #[test]
    fn decode_rejects_truncated() {
        // Claims root_len = 100 but no bytes follow.
        let mut bytes = 100u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"short");
        assert!(decode_config(&bytes).is_none());
    }

    #[test]
    fn decode_rejects_too_short_for_header() {
        assert!(decode_config(&[0u8, 1]).is_none());
    }

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

    #[test]
    fn bootstrap_garbage_config_is_bad_config() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("vfs-shim-badcfg-{}.bin", std::process::id()));
        std::fs::write(&path, [0u8, 1]).unwrap(); // too short for the header
        assert!(matches!(
            bootstrap_from_config_path(path.to_str().unwrap()),
            Err(BootstrapError::BadConfig)
        ));
        let _ = std::fs::remove_file(&path);
    }
}
