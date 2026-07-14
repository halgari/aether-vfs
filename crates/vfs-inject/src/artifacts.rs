//! Locate and co-locate dual-layer PE artifacts (payload next to shim DLL).
#![deny(unsafe_code)]

use std::path::{Path, PathBuf};

/// Find `name` near a reference path (same dir, parent, parent/deps).
pub fn find_near(reference: &Path, name: &str) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = reference.parent() {
        dirs.push(d.to_path_buf());
        dirs.push(d.join("deps"));
        if let Some(p) = d.parent() {
            dirs.push(p.to_path_buf());
            dirs.push(p.join("deps"));
        }
    }
    for d in dirs {
        let c = d.join(name);
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

/// Ensure `vfs_payload.dll` sits beside `shim_dll` (copy if found elsewhere).
/// Returns the path to use for dual-layer inject.
pub fn ensure_payload_beside_shim(shim_dll: &str, preferred_payload: Option<&str>) -> Option<String> {
    let shim = Path::new(shim_dll);
    let dir = shim.parent()?;
    let beside = dir.join("vfs_payload.dll");

    if beside.is_file() {
        return Some(beside.to_string_lossy().into_owned());
    }

    // Prefer explicit path if it exists.
    if let Some(p) = preferred_payload {
        let src = Path::new(p);
        if src.is_file() {
            if let Err(e) = std::fs::copy(src, &beside) {
                // If copy fails (e.g. cross-volume + locked), still return src.
                let _ = e;
                return Some(src.to_string_lossy().into_owned());
            }
            return Some(beside.to_string_lossy().into_owned());
        }
    }

    // Search near the shim, then near this process's exe (test layout).
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(c) = find_near(shim, "vfs_payload.dll") {
        candidates.push(c);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(c) = find_near(&exe, "vfs_payload.dll") {
            candidates.push(c);
        }
    }
    if let Ok(p) = std::env::var("VFS_PAYLOAD_PATH") {
        let c = PathBuf::from(p);
        if c.is_file() {
            candidates.push(c);
        }
    }

    for src in candidates {
        if src == beside {
            return Some(beside.to_string_lossy().into_owned());
        }
        if std::fs::copy(&src, &beside).is_ok() && beside.is_file() {
            return Some(beside.to_string_lossy().into_owned());
        }
        // Copy failed; usable absolute path is still fine for director inject.
        if src.is_file() {
            return Some(src.to_string_lossy().into_owned());
        }
    }
    None
}

/// Resolve payload path for `run_target_with_shim`: prefer configured path,
/// then co-locate beside the full shim DLL.
pub fn resolve_payload_for_run(payload_path: &str, shim_dll: &str) -> Option<String> {
    if Path::new(payload_path).is_file() {
        // Best-effort co-locate for any later child inject from this target.
        let _ = ensure_payload_beside_shim(shim_dll, Some(payload_path));
        return Some(payload_path.to_string());
    }
    ensure_payload_beside_shim(shim_dll, None)
}
