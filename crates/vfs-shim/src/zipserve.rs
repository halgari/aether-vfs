//! Serve zip-window bytes from memory-mapped container files behind synthetic
//! file handles. All `unsafe` for mapping lives here.
#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, FILE_MAP_READ, PAGE_READONLY,
};

/// High tag bit (2^46) marking a synthetic handle; real kernel handles never
/// reach this magnitude. The sign bit (2^63) stays clear so the value is a
/// positive handle, never confused with pseudo-handles (-1..-6) or
/// INVALID_HANDLE_VALUE.
const SYNTH_TAG: usize = 0x0000_4000_0000_0000;

/// A mapped container: base address of the whole-file view.
struct ZipMap {
    base: usize,
}

/// A synthetic open: absolute window start (map base + entry offset), length,
/// and current read position.
struct SynthFile {
    window: usize,
    length: u64,
    position: u64,
}

// Raw addresses stored as usize -> Send/Sync-safe in the maps.
static ZIP_MAPS: Mutex<BTreeMap<String, ZipMap>> = Mutex::new(BTreeMap::new());
static SYNTH: Mutex<BTreeMap<usize, SynthFile>> = Mutex::new(BTreeMap::new());
static NEXT_SLOT: Mutex<usize> = Mutex::new(0);

/// Strip a `\??\` / `\\?\` device prefix to a Win32 path for `CreateFileW`.
fn to_win32(nt: &str) -> String {
    nt.strip_prefix(r"\??\").or_else(|| nt.strip_prefix(r"\\?\")).unwrap_or(nt).to_string()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Map `container_nt` once (cached), returning its base address.
fn ensure_mapped(container_nt: &str) -> Option<usize> {
    let mut maps = ZIP_MAPS.lock().ok()?;
    if let Some(m) = maps.get(container_nt) {
        return Some(m.base);
    }
    let win = to_win32(container_nt);
    // SAFETY: standard read-only open + whole-file mapping; handles closed on
    // failure. The view outlives the process (never unmapped).
    unsafe {
        let path = wide(&win);
        let file = CreateFileW(
            path.as_ptr(),
            0x8000_0000, // GENERIC_READ
            FILE_SHARE_READ,
            core::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            core::ptr::null_mut(),
        );
        if file == INVALID_HANDLE_VALUE {
            return None;
        }
        let mapping = CreateFileMappingW(
            file,
            core::ptr::null(),
            PAGE_READONLY,
            0,
            0, // whole file
            core::ptr::null(),
        );
        if mapping.is_null() {
            CloseHandle(file);
            return None;
        }
        let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
        // The section keeps the pages alive while mapped; we can drop the file
        // and mapping handles but keep them for clarity — closing the file is
        // safe once the mapping exists. Keep the mapping handle open.
        CloseHandle(file);
        if view.Value.is_null() {
            CloseHandle(mapping);
            return None;
        }
        let base = view.Value as usize;
        maps.insert(container_nt.to_string(), ZipMap { base });
        Some(base)
    }
}

/// Register a synthetic open over `[offset, offset+length)` of `container_nt`.
pub fn open_synth(container_nt: &str, offset: u64, length: u64) -> Option<isize> {
    let base = ensure_mapped(container_nt)?;
    let window = base.checked_add(offset as usize)?;
    let mut slot = NEXT_SLOT.lock().ok()?;
    let handle = SYNTH_TAG | (*slot << 3);
    *slot += 1;
    drop(slot);
    SYNTH.lock().ok()?.insert(handle, SynthFile { window, length, position: 0 });
    Some(handle as isize)
}

/// Whether `handle` is one of ours.
pub fn is_synth(handle: isize) -> bool {
    (handle as usize) & SYNTH_TAG != 0
}

/// Read up to `want` bytes from `explicit_off` (or the current position).
/// Returns `(bytes, new_position, at_eof)`. `at_eof` is true when the read
/// started at or beyond the end (zero bytes available).
pub fn read(handle: isize, want: usize, explicit_off: Option<u64>) -> Option<(Vec<u8>, u64, bool)> {
    let mut t = SYNTH.lock().ok()?;
    let f = t.get_mut(&(handle as usize))?;
    let start = explicit_off.unwrap_or(f.position);
    if start >= f.length {
        return Some((Vec::new(), start, true));
    }
    let remaining = (f.length - start) as usize;
    let n = want.min(remaining);
    // SAFETY: window..window+length lies inside the mapped view; start+n <= length.
    let src = (f.window + start as usize) as *const u8;
    let bytes = unsafe { core::slice::from_raw_parts(src, n).to_vec() };
    let new_pos = start + n as u64;
    f.position = new_pos;
    Some((bytes, new_pos, false))
}

pub fn size(handle: isize) -> Option<u64> {
    Some(SYNTH.lock().ok()?.get(&(handle as usize))?.length)
}

pub fn position(handle: isize) -> Option<u64> {
    Some(SYNTH.lock().ok()?.get(&(handle as usize))?.position)
}

pub fn set_position(handle: isize, pos: u64) -> bool {
    match SYNTH.lock() {
        Ok(mut t) => match t.get_mut(&(handle as usize)) {
            Some(f) => {
                f.position = pos;
                true
            }
            None => false,
        },
        Err(_) => false,
    }
}

/// Drop a synthetic open. The container mapping stays cached for the process.
pub fn close(handle: isize) -> bool {
    match SYNTH.lock() {
        Ok(mut t) => t.remove(&(handle as usize)).is_some(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn serves_a_window_from_a_real_file() {
        // Build a file whose bytes 5..10 are the window; map + read it.
        let dir = std::env::temp_dir().join(format!("vfs-zipserve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.bin");
        std::fs::File::create(&path).unwrap().write_all(b"AAAAABCDEFGHIJ").unwrap();
        let nt = format!(r"\??\{}", path.to_string_lossy());

        let h = open_synth(&nt, 5, 5).expect("open_synth");
        assert!(is_synth(h));
        assert_eq!(size(h), Some(5));
        let (bytes, pos, eof) = read(h, 3, None).unwrap();
        assert_eq!(&bytes, b"BCD");
        assert_eq!(pos, 3);
        assert!(!eof);
        let (bytes2, _, _) = read(h, 100, None).unwrap();
        assert_eq!(&bytes2, b"EF"); // clamped to the 5-byte window
        let (_, _, eof2) = read(h, 1, None).unwrap();
        assert!(eof2);
        assert!(close(h));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
