//! File-mapping-backed shared memory. All Win32 FFI is confined here.

use std::io;

use vfs_ipc::SharedSeg;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

/// RAII owner of a Windows file-mapping section and its mapped read/write view.
///
/// The mapped view is exposed as a [`SharedSeg`] so the OS-independent ring and
/// snapshot code operate on real cross-process shared memory.
pub struct SharedMapping {
    handle: HANDLE,
    view: *mut core::ffi::c_void,
    len: usize,
    seg: SharedSeg,
}

// SAFETY: the mapped pages are shared memory; all concurrent access is governed
// by the vfs-ipc ring protocol (atomics + seqlock), the same rationale that
// makes `SharedSeg` itself `Send + Sync`.
#[allow(unsafe_code)]
unsafe impl Send for SharedMapping {}
#[allow(unsafe_code)]
unsafe impl Sync for SharedMapping {}

impl SharedMapping {
    /// Create a new named page-file-backed section of at least `size` bytes and
    /// map a read/write view. If a section of `name` already exists,
    /// `CreateFileMappingW` opens it (callers coordinate names to avoid this).
    pub fn create(name: &str, size: usize) -> io::Result<Self> {
        let wide = to_wide(name);
        let (size_high, size_low) = split_size(size)?;
        // SAFETY: FFI. INVALID_HANDLE_VALUE => page-file backing; wide is a valid
        // NUL-terminated UTF-16 pointer living for the call.
        #[allow(unsafe_code)]
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                core::ptr::null(),
                PAGE_READWRITE,
                size_high,
                size_low,
                wide.as_ptr(),
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Self::map_view(handle, size)
    }

    /// Map a read/write view of `handle` (which the constructor owns from here
    /// on) and wrap it as a `SharedSeg`. On failure the handle is closed.
    fn map_view(handle: HANDLE, size: usize) -> io::Result<Self> {
        // SAFETY: FFI. `handle` is a valid mapping handle; mapping the whole
        // section (offset 0, `size` bytes).
        #[allow(unsafe_code)]
        let view: MEMORY_MAPPED_VIEW_ADDRESS =
            unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        if view.Value.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: FFI. `handle` is valid; best-effort cleanup.
            #[allow(unsafe_code)]
            unsafe {
                CloseHandle(handle);
            }
            return Err(err);
        }
        let ptr = view.Value as *mut u8;
        // SAFETY: `ptr` is valid for `size` bytes for this mapping's lifetime and
        // is page-aligned (64 KB), satisfying the ring's 8-byte atomics.
        #[allow(unsafe_code)]
        let seg = unsafe { SharedSeg::from_raw(ptr, size) };
        Ok(Self {
            handle,
            view: view.Value,
            len: size,
            seg,
        })
    }

    /// The mapped view as a `SharedSeg`.
    pub fn seg(&self) -> &SharedSeg {
        &self.seg
    }

    /// The mapped length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the mapping is zero-length (never true for a live mapping).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for SharedMapping {
    fn drop(&mut self) {
        // SAFETY: FFI. `view`/`handle` were produced by MapViewOfFile /
        // CreateFileMappingW and are unmapped/closed exactly once here.
        #[allow(unsafe_code)]
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.view });
            CloseHandle(self.handle);
        }
    }
}

/// Convert a `&str` to a NUL-terminated UTF-16 buffer for the `*W` APIs.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

/// Split a `usize` size into the (high, low) 32-bit halves the mapping APIs take.
fn split_size(size: usize) -> io::Result<(u32, u32)> {
    let size = size as u64;
    Ok(((size >> 32) as u32, (size & 0xFFFF_FFFF) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A unique-ish section name without an OS randomness crate: derive from the
    // current process id and a per-test discriminator.
    fn section_name(tag: &str) -> String {
        let pid = std::process::id();
        format!("Local\\vfs-win-test-{pid}-{tag}")
    }

    #[test]
    fn create_maps_a_writable_section() {
        let m = SharedMapping::create(&section_name("create"), 64 * 1024).unwrap();
        assert_eq!(m.len(), 64 * 1024);
        // Initializing a ring writes the MAGIC/geometry into the mapped view and
        // requires an 8-aligned base for its atomics; success proves the section
        // is writable and correctly aligned.
        vfs_ipc::ring::init(m.seg(), 4, 256).unwrap();
    }
}
