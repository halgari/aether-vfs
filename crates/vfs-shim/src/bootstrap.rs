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

/// Read a config file, build an `Engine`, and install the NtCreateFile hook.
/// Returns the guard keeping the hook alive (the injected DLL leaks it).
pub fn bootstrap_from_config_path(path: &str) -> Result<HookGuard, BootstrapError> {
    let bytes = std::fs::read(path).map_err(|_| BootstrapError::Io)?;
    let (root, snapshot) = decode_config(&bytes).ok_or(BootstrapError::BadConfig)?;
    let engine = Engine::new(&root, snapshot).map_err(BootstrapError::Engine)?;
    let guard = install(engine).map_err(BootstrapError::Install)?;
    // Tell any spawning parent (that force-suspended us) our hooks are live.
    crate::inject::signal_ready();
    Ok(guard)
}

/// Encode `[u32 LE root_len][root utf8][snapshot bytes]`.
pub fn encode_config(root: &str, snapshot: &[u8]) -> Vec<u8> {
    let root = root.as_bytes();
    let mut out = Vec::with_capacity(4 + root.len() + snapshot.len());
    out.extend_from_slice(&(root.len() as u32).to_le_bytes());
    out.extend_from_slice(root);
    out.extend_from_slice(snapshot);
    out
}

/// Decode a buffer produced by [`encode_config`]. Returns `None` on truncation or
/// invalid UTF-8 in the root. Never panics.
pub fn decode_config(bytes: &[u8]) -> Option<(String, Vec<u8>)> {
    if bytes.len() < 4 {
        return None;
    }
    let root_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let root_end = 4usize.checked_add(root_len)?;
    if bytes.len() < root_end {
        return None;
    }
    let root = std::str::from_utf8(&bytes[4..root_end]).ok()?.to_string();
    let snapshot = bytes[root_end..].to_vec();
    Some((root, snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips() {
        let snapshot = vec![1u8, 2, 3, 4, 5];
        let bytes = encode_config(r"\??\C:\Games\Skyrim", &snapshot);
        let (root, snap) = decode_config(&bytes).unwrap();
        assert_eq!(root, r"\??\C:\Games\Skyrim");
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
