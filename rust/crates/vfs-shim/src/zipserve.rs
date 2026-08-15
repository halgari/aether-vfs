//! Serve zip-window bytes from memory-mapped container files behind synthetic
//! file handles. All `unsafe` for mapping lives here.
#![allow(unsafe_code)]

use std::collections::BTreeMap;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileSizeEx, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS,
    PAGE_READONLY,
};

/// High tag bit (2^46) marking a synthetic *file* handle; real kernel handles
/// never reach this magnitude. The sign bit (2^63) stays clear so the value is
/// a positive handle, never confused with pseudo-handles (-1..-6) or
/// INVALID_HANDLE_VALUE.
const SYNTH_TAG: usize = 0x0000_4000_0000_0000;
/// Tag bit (2^45) for synthetic *section* handles (NtCreateSection results).
const SYNTH_SECTION_TAG: usize = 0x0000_2000_0000_0000;

/// A mapped container: base address of the whole-file view and the
/// container's byte size (captured at mapping time, used to bound windows).
struct ZipMap {
    base: usize,
    size: u64,
}

/// A synthetic open: absolute window start (map base + entry offset), length,
/// and current read position.
struct SynthFile {
    window: usize,
    length: u64,
    position: u64,
}

/// A synthetic section over a zip-window (data mapping only, not SEC_IMAGE).
struct SynthSection {
    window: usize,
    length: u64,
}

// Raw addresses stored as usize -> Send/Sync-safe in the maps.
static ZIP_MAPS: Mutex<BTreeMap<String, ZipMap>> = Mutex::new(BTreeMap::new());
static SYNTH: Mutex<BTreeMap<usize, SynthFile>> = Mutex::new(BTreeMap::new());
static SYNTH_SECTIONS: Mutex<BTreeMap<usize, SynthSection>> = Mutex::new(BTreeMap::new());
/// One outstanding synthetic view. Callers may map the same base twice (two
/// views at the same section offset), so keep a refcount rather than letting
/// the first unmap forget a base the process is still using.
struct SynthView {
    length: u64,
    refs: u32,
}

/// Mapped synthetic views: base address → view (for UnmapView no-op).
static SYNTH_VIEWS: Mutex<BTreeMap<usize, SynthView>> = Mutex::new(BTreeMap::new());
static NEXT_SLOT: Mutex<usize> = Mutex::new(0);

/// Strip a `\??\` / `\\?\` device prefix to a Win32 path for `CreateFileW`.
fn to_win32(nt: &str) -> String {
    nt.strip_prefix(r"\??\").or_else(|| nt.strip_prefix(r"\\?\")).unwrap_or(nt).to_string()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open + whole-file-map `win` without holding any lock. Returns the mapped
/// base address and the container's byte size, or `None` on any failure.
///
/// SAFETY: standard read-only open + whole-file mapping; handles closed on
/// failure. The view outlives the process (never unmapped) once installed —
/// callers that lose the insert race are responsible for unmapping their
/// duplicate view via `UnmapViewOfFile`.
unsafe fn map_container(win: &str) -> Option<(usize, u64)> {
    let path = wide(win);
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
    let mut size: i64 = 0;
    if GetFileSizeEx(file, &mut size) == 0 || size < 0 {
        CloseHandle(file);
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
    // and mapping handles once the view exists — closing them is safe once
    // the mapping/view exist.
    CloseHandle(file);
    if view.Value.is_null() {
        CloseHandle(mapping);
        return None;
    }
    // The view now holds the section's pages alive; the mapping handle is no
    // longer needed and would otherwise leak one handle per mapped container.
    CloseHandle(mapping);
    Some((view.Value as usize, size as u64))
}

/// Map `container_nt` once (cached), returning its base address and byte
/// size. The `ZIP_MAPS` lock is never held across the mapping syscalls:
/// `CreateFileW` funnels through the process's hooked `NtCreateFile`, so
/// holding the lock across it would be a reentrant deadlock.
fn ensure_mapped(container_nt: &str) -> Option<(usize, u64)> {
    // Fast path: already mapped.
    {
        let maps = ZIP_MAPS.lock().ok()?;
        if let Some(m) = maps.get(container_nt) {
            return Some((m.base, m.size));
        }
    }
    let win = to_win32(container_nt);
    // SAFETY: no lock held here; see `map_container`.
    let (base, size) = unsafe { map_container(&win) }?;
    let mut maps = ZIP_MAPS.lock().ok()?;
    if let Some(m) = maps.get(container_nt) {
        // Another thread won the race and already inserted this container;
        // release our duplicate view and use the existing one.
        // SAFETY: `base` is a view we just mapped and haven't shared with
        // anyone else, so unmapping it here is sound.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: base as *mut core::ffi::c_void });
        }
        return Some((m.base, m.size));
    }
    maps.insert(container_nt.to_string(), ZipMap { base, size });
    Some((base, size))
}

/// Register a synthetic open over `[offset, offset+length)` of `container_nt`.
///
/// **Unreferenced since `Decision::Serve` went** — the two `hook.rs` arms that
/// called it were its only callers. Marked rather than deleted here purely to
/// keep this commit warning-clean; the very next commit removes it with the
/// rest of the zip-window half. Same marker, same meaning, as the one on
/// `copy_window_to_file` below.
#[allow(dead_code)]
pub fn open_synth(container_nt: &str, offset: u64, length: u64) -> Option<isize> {
    let (base, file_size) = ensure_mapped(container_nt)?;
    let end = offset.checked_add(length)?;
    if end > file_size {
        return None;
    }
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

/// Whether `handle` is a synthetic section (from [`create_section`]).
pub fn is_synth_section(handle: isize) -> bool {
    (handle as usize) & SYNTH_SECTION_TAG != 0
}

/// Create a data section over a synthetic file handle's window.
/// Returns `None` if `file_handle` is not a known synthetic file.
pub fn create_section(file_handle: isize) -> Option<isize> {
    let (window, length) = {
        let t = SYNTH.lock().ok()?;
        let f = t.get(&(file_handle as usize))?;
        (f.window, f.length)
    };
    let mut slot = NEXT_SLOT.lock().ok()?;
    let handle = SYNTH_SECTION_TAG | (*slot << 3);
    *slot += 1;
    drop(slot);
    SYNTH_SECTIONS
        .lock()
        .ok()?
        .insert(handle, SynthSection { window, length });
    Some(handle as isize)
}

/// Register an already-mapped PE image (from `map_image_from_pe_bytes_local`)
/// as a synthetic section so `MapViewOfSection` returns `base`.
pub fn register_mapped_image(base: usize, size: u64) -> Option<isize> {
    let mut slot = NEXT_SLOT.lock().ok()?;
    let handle = SYNTH_SECTION_TAG | (*slot << 3);
    *slot += 1;
    drop(slot);
    SYNTH_SECTIONS
        .lock()
        .ok()?
        .insert(handle, SynthSection { window: base, length: size });
    Some(handle as isize)
}

/// Close a synthetic section handle, returning the window base it covered.
///
/// The base lets the caller release shim-owned address space (see
/// [`crate::lazy_section::on_section_closed`]). Views may still be outstanding;
/// the owner frees only once the last one is unmapped.
pub fn close_section(handle: isize) -> Option<usize> {
    SYNTH_SECTIONS
        .lock()
        .ok()?
        .remove(&(handle as usize))
        .map(|s| s.window)
}

/// Map a view of a synthetic section into the current process.
///
/// Returns `(base, view_size)` on success. The base is a pointer into the
/// already-mapped zip container (no extra mapping); [`unmap_view`] must be used
/// so we do not `UnmapViewOfFile` the shared container.
pub fn map_view(
    section_handle: isize,
    section_offset: u64,
    view_size: u64,
) -> Option<(usize, u64)> {
    let t = SYNTH_SECTIONS.lock().ok()?;
    let s = t.get(&(section_handle as usize))?;
    if section_offset > s.length {
        return None;
    }
    let max = s.length - section_offset;
    let size = if view_size == 0 { max } else { view_size.min(max) };
    if size == 0 && max == 0 {
        // Empty file: still "succeed" with a non-null? Prefer fail.
        return None;
    }
    let base = s.window.checked_add(section_offset as usize)?;
    drop(t);
    if let Ok(mut views) = SYNTH_VIEWS.lock() {
        views
            .entry(base)
            .and_modify(|v| {
                v.refs += 1;
                v.length = v.length.max(size);
            })
            .or_insert(SynthView { length: size, refs: 1 });
    }
    Some((base, size))
}

/// Whether any synthetic view still falls inside `[base, base+len)`.
///
/// Owners of shim-allocated section memory use this to decide when a region is
/// unreferenced. On a poisoned lock this reports "still in use": leaking a
/// reservation is recoverable, freeing one out from under a live view is not.
pub fn has_view_in(base: usize, len: usize) -> bool {
    match SYNTH_VIEWS.lock() {
        Ok(v) => v.range(base..base.saturating_add(len)).next().is_some(),
        Err(_) => true,
    }
}

/// Whether `base` is a synthetic mapped view (should no-op on UnmapView).
pub fn is_synth_view(base: usize) -> bool {
    SYNTH_VIEWS
        .lock()
        .map(|v| v.contains_key(&base))
        .unwrap_or(false)
}

/// Forget one reference to a synthetic mapped view (do not unmap the underlying
/// zip). Returns true when this was the *last* reference to `base`.
pub fn unmap_view(base: usize) -> bool {
    let Ok(mut views) = SYNTH_VIEWS.lock() else {
        return false;
    };
    match views.get_mut(&base) {
        Some(v) if v.refs > 1 => {
            v.refs -= 1;
            false
        }
        Some(_) => {
            views.remove(&base);
            true
        }
        None => false,
    }
}

/// Copy `[offset, offset+length)` of a container into `dest` (for COW overlay).
/// Maps the container if needed. Returns false on any failure.
///
/// **Unreferenced since gate 4 task 4.** Its only caller was
/// `Engine::cow_seed`'s `Decision::Serve` arm, which seeded copy-up straight
/// out of a zip window without asking the director; copy-up now reads through
/// the ring instead. Left standing (rather than deleted here) because gate 4
/// task 7 removes `Decision::Serve`, `zipserve`, and this function together —
/// this `allow` is the marker for that, not a reprieve.
#[allow(dead_code)]
pub fn copy_window_to_file(container_nt: &str, offset: u64, length: u64, dest: &std::path::Path) -> bool {
    let (base, file_size) = match ensure_mapped(container_nt) {
        Some(x) => x,
        None => return false,
    };
    let end = match offset.checked_add(length) {
        Some(e) if e <= file_size => e,
        _ => return false,
    };
    let _ = end;
    let src = (base + offset as usize) as *const u8;
    // SAFETY: window lies inside the mapped container (bounds checked above).
    let bytes = unsafe { core::slice::from_raw_parts(src, length as usize) };
    if let Some(parent) = dest.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(dest, bytes).is_ok()
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

    #[test]
    fn rejects_a_window_past_the_container_end() {
        let dir = std::env::temp_dir().join(format!("vfs-zipserve-oob-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("small.bin");
        std::fs::write(&path, b"0123456789").unwrap(); // 10 bytes
        let nt = format!(r"\??\{}", path.to_string_lossy());
        // A window that runs past EOF must be rejected, not served.
        assert!(open_synth(&nt, 5, 100).is_none());
        // A window fully inside the file still works.
        let h = open_synth(&nt, 5, 5).expect("in-bounds window");
        assert_eq!(size(h), Some(5));
        assert!(close(h));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
