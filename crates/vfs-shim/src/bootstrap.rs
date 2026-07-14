//! Bootstrap glue: a tiny config codec and a config-file entry point used by the
//! injected DLL to build an `Engine` and install the hook. No `unsafe` here.

use crate::engine::{Engine, EngineError};
use crate::hook::{install, HookGuard, InstallError};

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
pub fn bootstrap_from_config_path(path: &str) -> Result<HookGuard, BootstrapError> {
    let bytes = std::fs::read(path).map_err(|_| BootstrapError::Io)?;
    let (root, overlay, snapshot) = decode_config(&bytes).ok_or(BootstrapError::BadConfig)?;
    let engine = if overlay.is_empty() {
        Engine::new(&root, snapshot)
    } else {
        Engine::with_overlay(&root, &overlay, snapshot)
    }
    .map_err(BootstrapError::Engine)?;
    let guard = install(engine).map_err(BootstrapError::Install)?;
    // Tell any spawning parent (that force-suspended us) our hooks are live.
    crate::inject::signal_ready();
    Ok(guard)
}

/// Encode with no write overlay. See [`encode_config_with_overlay`].
pub fn encode_config(root: &str, snapshot: &[u8]) -> Vec<u8> {
    encode_config_with_overlay(root, "", snapshot)
}

/// Encode `[u32 root_len][root][u32 overlay_len][overlay][snapshot]` (all UTF-8).
/// An empty `overlay` disables the write path.
pub fn encode_config_with_overlay(root: &str, overlay: &str, snapshot: &[u8]) -> Vec<u8> {
    let root = root.as_bytes();
    let overlay = overlay.as_bytes();
    let mut out = Vec::with_capacity(8 + root.len() + overlay.len() + snapshot.len());
    out.extend_from_slice(&(root.len() as u32).to_le_bytes());
    out.extend_from_slice(root);
    out.extend_from_slice(&(overlay.len() as u32).to_le_bytes());
    out.extend_from_slice(overlay);
    out.extend_from_slice(snapshot);
    out
}

/// Decode a buffer produced by [`encode_config_with_overlay`]. Returns
/// `(root, overlay, snapshot)`; `overlay` empty means none. `None` on truncation
/// or invalid UTF-8. Never panics.
pub fn decode_config(bytes: &[u8]) -> Option<(String, String, Vec<u8>)> {
    let read_field = |b: &[u8], off: usize| -> Option<(String, usize)> {
        let len = u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?) as usize;
        let start = off + 4;
        let end = start.checked_add(len)?;
        let s = std::str::from_utf8(b.get(start..end)?).ok()?.to_string();
        Some((s, end))
    };
    let (root, after_root) = read_field(bytes, 0)?;
    let (overlay, after_overlay) = read_field(bytes, after_root)?;
    let snapshot = bytes.get(after_overlay..)?.to_vec();
    Some((root, overlay, snapshot))
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
