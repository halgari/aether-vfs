//! File-mapping-backed shared memory. All Win32 FFI is confined here.

use std::io;
use std::path::Path;

use vfs_ipc::SharedSeg;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
    FILE_MAP_ALL_ACCESS, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
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

    /// Open an existing named section and map a read/write view.
    pub fn open(name: &str, size: usize) -> io::Result<Self> {
        let wide = to_wide(name);
        // SAFETY: FFI. `wide` is a valid NUL-terminated UTF-16 pointer for the
        // call; FALSE (0) => the mapped view handle is not inheritable.
        #[allow(unsafe_code)]
        let handle = unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, wide.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Self::map_view(handle, size)
    }

    /// Create (or truncate) `path` at exactly `size` bytes, then map a
    /// read/write view of a **file-backed** section over it.
    ///
    /// The difference from [`Self::create`] is the first argument to
    /// `CreateFileMappingW`: a real file handle instead of
    /// `INVALID_HANDLE_VALUE`. That is the whole point — a page-file-backed
    /// section exists only inside one Windows (or Wine) session and has no
    /// identity a native Linux process can open, whereas a file-backed one is
    /// coherent with an `mmap` of the same path. Measured: a Wine process and a
    /// Linux process each saw the other's writes through this arrangement.
    ///
    /// The section is unnamed. Callers coordinate by **path**, not by section
    /// name, which is what lets the two sides agree across the boundary.
    pub fn create_file_backed(path: &Path, size: usize) -> io::Result<Self> {
        let file = open_backing(path, true)?;
        // The file must be exactly `size` bytes: the Linux side maps it by
        // length, and mapping past the end of a short file faults on touch
        // rather than failing at map time. `CreateFileMappingW` with a nonzero
        // size extends the file, but do it explicitly so the postcondition is
        // visible and testable.
        let (size_high, size_low) = split_size(size)?;
        // SAFETY: FFI. `file` is a valid writable handle owned here; passing it
        // as the mapping's backing store. `size` is nonzero for any real ring.
        #[allow(unsafe_code)]
        let handle = unsafe {
            CreateFileMappingW(
                file,
                core::ptr::null(),
                PAGE_READWRITE,
                size_high,
                size_low,
                core::ptr::null(),
            )
        };
        // The mapping holds its own reference to the file object, so the file
        // handle is closed here and the pages stay valid.
        // SAFETY: FFI. `file` is valid and closed exactly once.
        #[allow(unsafe_code)]
        unsafe {
            CloseHandle(file);
        }
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Self::map_view(handle, size)
    }

    /// Map a read/write view of an existing file at `path`, which must already
    /// be at least `size` bytes. See [`Self::create_file_backed`].
    ///
    /// A shorter file is **refused**. `CreateFileMappingW` with a nonzero size
    /// would otherwise extend the file to `size` and return success, which
    /// contradicts the "must already be" above and diverges from
    /// `vfs_unix::FileMapping::open`, the deliberate mirror of this function.
    /// The divergence matters across the boundary: silently growing a short
    /// file hands the ring code a zero-filled segment, which surfaces as an
    /// `IpcError::Layout` about a missing magic rather than as the length
    /// mismatch it actually is.
    pub fn open_file_backed(path: &Path, size: usize) -> io::Result<Self> {
        // Checked before the mapping is created, not after: once
        // `CreateFileMappingW` succeeds the file has already been extended, and
        // an error returned at that point leaves the damage behind.
        let actual = std::fs::metadata(path)?.len();
        if actual < size as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("backing file is {actual} bytes, need at least {size}"),
            ));
        }
        let file = open_backing(path, false)?;
        let (size_high, size_low) = split_size(size)?;
        // SAFETY: FFI. As in `create_file_backed`.
        #[allow(unsafe_code)]
        let handle = unsafe {
            CreateFileMappingW(
                file,
                core::ptr::null(),
                PAGE_READWRITE,
                size_high,
                size_low,
                core::ptr::null(),
            )
        };
        // SAFETY: FFI. `file` is valid and closed exactly once.
        #[allow(unsafe_code)]
        unsafe {
            CloseHandle(file);
        }
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

    /// Raw start of the mapped view (for carving an arena after the ring).
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.view as *mut u8
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

/// Open `path` for shared read/write. `create` truncates or creates; otherwise
/// the file must exist. Shared read+write so the other side can map it
/// concurrently — without `FILE_SHARE_*` the second opener gets a sharing
/// violation, which is exactly the case this transport exists to support.
///
/// `to_string_lossy` is acceptable here because ring paths are chosen by this
/// codebase, not by a user. If a caller ever passes a non-UTF-8 path this
/// silently substitutes replacement characters and the open fails with a
/// confusing error -- if that becomes a real risk, switch to
/// `std::os::windows::ffi::OsStrExt::encode_wide`.
fn open_backing(path: &Path, create: bool) -> io::Result<HANDLE> {
    let wide = to_wide(&path.to_string_lossy());
    let disposition = if create { CREATE_ALWAYS } else { OPEN_EXISTING };
    // SAFETY: FFI. `wide` is a valid NUL-terminated UTF-16 pointer for the call.
    #[allow(unsafe_code)]
    let file = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            core::ptr::null(),
            disposition,
            FILE_ATTRIBUTE_NORMAL,
            core::ptr::null_mut(),
        )
    };
    if file == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
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

    #[test]
    fn open_aliases_the_same_section() {
        let name = section_name("alias");
        let creator = SharedMapping::create(&name, 64 * 1024).unwrap();
        // Creator writes the ring MAGIC + geometry into the shared pages.
        let geom_created = vfs_ipc::ring::init(creator.seg(), 4, 256).unwrap();
        // A second mapping of the SAME section sees those bytes: ring::open validates
        // the MAGIC the creator wrote and recovers the identical geometry.
        let opener = SharedMapping::open(&name, 64 * 1024).unwrap();
        let geom_opened = vfs_ipc::ring::open(opener.seg()).unwrap();
        assert_eq!(geom_created, geom_opened);
    }

    #[test]
    fn open_missing_section_errors() {
        let name = section_name("does-not-exist-xyz");
        let err = SharedMapping::open(&name, 64 * 1024);
        assert!(err.is_err());
    }

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let pid = std::process::id();
        std::env::temp_dir().join(format!("vfs-win-filemap-{pid}-{tag}.bin"))
    }

    #[test]
    fn file_backed_create_maps_a_writable_section() {
        let p = temp_path("create");
        let _ = std::fs::remove_file(&p);
        let m = SharedMapping::create_file_backed(&p, 64 * 1024).unwrap();
        assert_eq!(m.len(), 64 * 1024);
        // ring::init requires an 8-aligned writable base; success proves both.
        vfs_ipc::ring::init(m.seg(), 4, 256).unwrap();
        drop(m);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn file_backed_create_sizes_the_file_on_disk() {
        // The Linux side mmaps this file by length, so the file must actually be
        // `size` bytes long -- a sparse or zero-length file would give the
        // Director a SIGBUS on first touch rather than a clean error.
        let p = temp_path("sized");
        let _ = std::fs::remove_file(&p);
        let m = SharedMapping::create_file_backed(&p, 64 * 1024).unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        assert_eq!(meta.len(), 64 * 1024, "backing file must be fully sized");
        drop(m);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn file_backed_open_aliases_the_same_bytes() {
        let p = temp_path("alias");
        let _ = std::fs::remove_file(&p);
        let creator = SharedMapping::create_file_backed(&p, 64 * 1024).unwrap();
        let geom_created = vfs_ipc::ring::init(creator.seg(), 4, 256).unwrap();
        let opener = SharedMapping::open_file_backed(&p, 64 * 1024).unwrap();
        let geom_opened = vfs_ipc::ring::open(opener.seg()).unwrap();
        assert_eq!(geom_created, geom_opened);
        drop(opener);
        drop(creator);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn file_backed_writes_are_visible_through_a_second_mapping() {
        // Coherence, not just aliasing: a byte written through one view must be
        // readable through the other. This is the property the Wine/Linux split
        // depends on.
        let p = temp_path("coherent");
        let _ = std::fs::remove_file(&p);
        let a = SharedMapping::create_file_backed(&p, 64 * 1024).unwrap();
        let b = SharedMapping::open_file_backed(&p, 64 * 1024).unwrap();
        // SAFETY: both views map the same 64 KiB file; writing one byte at a
        // fixed offset inside it, with no concurrent reader but `b` below.
        #[allow(unsafe_code)]
        unsafe {
            a.as_mut_ptr().add(4096).write_volatile(0xAB);
            assert_eq!(b.as_mut_ptr().add(4096).read_volatile(), 0xAB);
        }
        drop(b);
        drop(a);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn file_backed_open_missing_file_errors() {
        let p = temp_path("absent-xyz");
        let _ = std::fs::remove_file(&p);
        assert!(SharedMapping::open_file_backed(&p, 64 * 1024).is_err());
    }

    #[test]
    fn file_backed_open_too_short_file_errors_rather_than_growing_it() {
        let p = temp_path("short");
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, [0u8; 128]).unwrap();
        assert!(SharedMapping::open_file_backed(&p, 64 * 1024).is_err());
        assert_eq!(
            std::fs::metadata(&p).unwrap().len(),
            128,
            "a refused open must not have grown the file"
        );
        let _ = std::fs::remove_file(&p);
    }
}
