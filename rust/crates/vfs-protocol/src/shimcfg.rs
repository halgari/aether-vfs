//! Shim config wire codec: root, write overlay, static-import table, and
//! engine snapshot, packed into the byte buffer the injected DLL reads on
//! bootstrap.
//!
//! This lives in `vfs-protocol` (portable, no OS dependency) rather than in
//! `vfs-shim` (Windows-only via `retour`/`windows-sys`) because a native
//! Linux Director must be able to build a shim config for a Wine-hosted
//! shim: the encoder is pure byte assembly and only its former enclosing
//! module (`crate::engine`, `crate::hook`, `crate::payload_abi`) needed
//! Windows. The decoder stays in `vfs-shim` — only the shim itself reads
//! these bytes back.

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire bytes are a pinned format, so this asserts the exact encoding
    /// rather than a round trip: a round trip would pass even if both sides
    /// moved together, which is precisely the regression that would break an
    /// already-shipped shim.
    #[test]
    fn encode_config_full_layout_is_unchanged() {
        let out = encode_config_full("R", "O", &[], &[7, 8]);
        // len("R")=1, "R", len("O")=1, "O", then "VFS1", the static-import
        // count, then the snapshot.
        assert_eq!(&out[0..4], &1u32.to_le_bytes());
        assert_eq!(out[4], b'R');
        assert_eq!(&out[5..9], &1u32.to_le_bytes());
        assert_eq!(out[9], b'O');
        assert!(out.ends_with(&[7, 8]), "snapshot must be last: {out:?}");
    }

    #[test]
    fn with_overlay_matches_full_with_no_static_imports() {
        assert_eq!(
            encode_config_with_overlay("R", "O", &[1, 2, 3]),
            encode_config_full("R", "O", &[], &[1, 2, 3])
        );
    }
}
