//! Locate built PE artifacts, and tell an import-activated EXE from one to inject.
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

/// Whether the EXE at `path` imports the shim first ([`vfs_pe::SHIM_IMPORT_DLL`]):
/// staging patched it, so it activates the VFS itself and must not be
/// injected (that would load a second copy of the shim). Reads the headers
/// only. Unreadable or malformed reads as "no", which means "inject".
pub fn exe_imports_shim(path: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut read_at = |off: u64, len: usize| -> Option<Vec<u8>> {
        f.seek(SeekFrom::Start(off)).ok()?;
        let mut buf = Vec::with_capacity(len);
        (&mut f).take(len as u64).read_to_end(&mut buf).ok()?;
        Some(buf)
    };
    vfs_pe::first_import_is(&mut read_at, vfs_pe::SHIM_IMPORT_DLL)
}
