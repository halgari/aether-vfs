//! Parse the config-file static-import table (shared wire format with vfs-shim).
//! Kept here so vfs-inject does not depend on vfs-shim (avoids a dep cycle:
//! vfs-shim → vfs-inject for child dual-layer).

use crate::payload_cfg::MAX_REDIRECTS;
use crate::PreinitRedirect;

const CONFIG_MAGIC: &[u8; 4] = b"VFS1";

/// One static-import row from the config file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaticImport {
    pub dll_name: String,
    pub backing_path: String,
}

fn read_field(b: &[u8], off: usize) -> Option<(String, usize)> {
    let len = u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?) as usize;
    let start = off + 4;
    let end = start.checked_add(len)?;
    let s = std::str::from_utf8(b.get(start..end)?).ok()?.to_string();
    Some((s, end))
}

/// Decode static imports from a full config blob (same layout as vfs-shim).
pub fn decode_static_imports(bytes: &[u8]) -> Option<Vec<StaticImport>> {
    let (_root, after_root) = read_field(bytes, 0)?;
    let (_overlay, after_overlay) = read_field(bytes, after_root)?;
    let rest = bytes.get(after_overlay..)?;
    if rest.len() < 4 || &rest[..4] != CONFIG_MAGIC {
        return Some(Vec::new());
    }
    let mut off = 4usize;
    let n = u32::from_le_bytes(rest.get(off..off + 4)?.try_into().ok()?) as usize;
    off += 4;
    let mut statics = Vec::with_capacity(n.min(MAX_REDIRECTS));
    for _ in 0..n {
        let (name, o1) = read_field(rest, off)?;
        let (backing, o2) = read_field(rest, o1)?;
        off = o2;
        statics.push(StaticImport {
            dll_name: name,
            backing_path: backing,
        });
    }
    Some(statics)
}

pub fn load_static_imports_from_path(path: &str) -> Option<Vec<StaticImport>> {
    let bytes = std::fs::read(path).ok()?;
    decode_static_imports(&bytes)
}

/// Convert static-import rows into early-payload redirects (stat backings, NT paths).
pub fn static_imports_to_preinit(statics: &[StaticImport], max: usize) -> Vec<PreinitRedirect> {
    let mut out = Vec::new();
    for e in statics.iter().take(max) {
        let path = e.backing_path.trim();
        if path.is_empty() || e.dll_name.trim().is_empty() {
            continue;
        }
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
        let suffix = e
            .dll_name
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(e.dll_name.as_str())
            .to_string();
        out.push(PreinitRedirect {
            suffix,
            backing_nt,
            backing_size: size,
        });
    }
    out
}

pub fn load_preinit_from_config_file(path: &str, max: usize) -> Vec<PreinitRedirect> {
    match load_static_imports_from_path(path) {
        Some(s) => static_imports_to_preinit(&s, max),
        None => Vec::new(),
    }
}
