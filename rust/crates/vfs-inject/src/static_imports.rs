//! Read the config-file static-import table (the codec is `vfs_protocol::shimcfg`, shared with
//! vfs-shim, which depends on vfs-inject for child dual-layer).

use crate::PreinitRedirect;

pub use vfs_protocol::shimcfg::StaticImport;

/// Static imports from a config file. `None` when the file is unreadable or does not decode
/// (including a config from another build: the shim's bootstrap refuses that by name).
pub(crate) fn load_static_imports_from_path(path: &str) -> Option<Vec<StaticImport>> {
    let bytes = std::fs::read(path).ok()?;
    Some(vfs_protocol::shimcfg::decode_config(&bytes).ok()?.static_imports)
}

/// Convert static-import rows into early-payload redirects (stat backings, NT paths).
pub(crate) fn static_imports_to_preinit(
    statics: &[StaticImport],
    max: usize,
) -> Vec<PreinitRedirect> {
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

pub(crate) fn load_preinit_from_config_file(path: &str, max: usize) -> Vec<PreinitRedirect> {
    match load_static_imports_from_path(path) {
        Some(s) => static_imports_to_preinit(&s, max),
        None => Vec::new(),
    }
}
