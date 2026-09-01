//! File-backed shared memory via `mmap`. All libc FFI is confined here.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

use vfs_ipc::SharedSeg;

/// RAII owner of an `mmap`ed region over a real file, exposed as a [`SharedSeg`].
///
/// The Unix counterpart of `vfs_win::SharedMapping`'s file-backed constructors.
/// Both sides agree by **path**: a Windows `CreateFileMappingW` over the same
/// file and this `mmap` are coherent, which is what lets a shim inside Wine and
/// a native Linux Director share one ring.
pub struct FileMapping {
    ptr: *mut u8,
    len: usize,
    seg: SharedSeg,
}

// SAFETY: the mapped pages are shared memory; all concurrent access is governed
// by the vfs-ipc ring protocol (atomics + seqlock) — the same rationale that
// makes `SharedSeg` itself `Send + Sync`.
#[allow(unsafe_code)]
unsafe impl Send for FileMapping {}
#[allow(unsafe_code)]
unsafe impl Sync for FileMapping {}

impl FileMapping {
    /// Create `path` if it does not exist, ensure it is **at least** `size`
    /// bytes, and map that many bytes shared.
    ///
    /// **Grow-only: an existing file longer than `size` is left at its current
    /// length, never truncated.** Truncating is not a tidiness question here.
    /// Another process may already hold a live `MAP_SHARED` mapping of this
    /// path — a second Director on the same ring file is the ordinary case —
    /// and shortening a file below a mapped length does not make that process's
    /// accesses fail, it makes them raise SIGBUS. So a `create` that truncated
    /// would kill the first Director from inside the second one's setup path.
    ///
    /// Stale content from a previous, longer occupant is not a concern:
    /// `vfs_ipc::ring::init` overwrites the header and geometry, and it is
    /// exactly that write which makes the segment usable to either side.
    pub fn create(path: &Path, size: usize) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        // Size the file before mapping. `mmap` past EOF succeeds and then
        // SIGBUSes on first touch, which would surface as a crash in the
        // Director rather than an error at setup. Grow only — see above.
        let actual = file.metadata()?.len();
        if actual < size as u64 {
            file.set_len(size as u64)?;
        }
        Self::map(&file, size)
    }

    /// Map an existing file at `path`, which must be at least `size` bytes.
    pub fn open(path: &Path, size: usize) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let actual = file.metadata()?.len();
        if actual < size as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("backing file is {actual} bytes, need at least {size}"),
            ));
        }
        Self::map(&file, size)
    }

    fn map(file: &std::fs::File, size: usize) -> io::Result<Self> {
        if size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "zero-length mapping",
            ));
        }
        // SAFETY: FFI. `fd` is a valid open read/write descriptor living for the
        // call; MAP_SHARED is required for cross-process coherence, which is the
        // entire purpose here. The kernel chooses the address (null hint).
        #[allow(unsafe_code)]
        let ptr = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = ptr as *mut u8;
        // SAFETY: `mmap` returned page-aligned memory, satisfying the ring's
        // 8-byte atomics, and the file descriptor may be closed now — the
        // mapping keeps its own reference to the underlying file object.
        //
        // `ptr` is valid for `size` bytes for this value's lifetime **provided
        // no process truncates the backing file below `size`**. That proviso is
        // not something this type can enforce, and it is not a soft one: a
        // `MAP_SHARED` mapping over a file shortened under it does not begin
        // returning errors, it delivers **SIGBUS** on the next touch of a page
        // beyond the new end of file. That is an uncatchable fault killing the
        // process, not an `io::Result` any caller could handle. Windows is
        // accidentally immune (`ERROR_USER_MAPPED_FILE` refuses the truncation);
        // here nothing refuses it.
        //
        // What upholds the proviso is therefore the protocol, not the code
        // below: `Self::create` is grow-only and never shrinks an existing file
        // (see its docs), and no other path in this crate calls `set_len` /
        // `ftruncate` on a mapped path. Any caller that shortens or replaces a
        // live ring file by some other means — `set_len`, `O_TRUNC`, a
        // `rename`-over followed by a shorter write — breaks this invariant and
        // crashes whoever still holds the mapping.
        #[allow(unsafe_code)]
        let seg = unsafe { SharedSeg::from_raw(ptr, size) };
        Ok(Self { ptr, len: size, seg })
    }

    /// The mapped region as a `SharedSeg`.
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

    /// Raw start of the mapped region (for carving an arena after the ring).
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }
}

impl Drop for FileMapping {
    fn drop(&mut self) {
        // SAFETY: FFI. `ptr`/`len` came from `mmap` above and are unmapped
        // exactly once here.
        #[allow(unsafe_code)]
        unsafe {
            libc::munmap(self.ptr as *mut core::ffi::c_void, self.len);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let pid = std::process::id();
        std::env::temp_dir().join(format!("vfs-unix-filemap-{pid}-{tag}.bin"))
    }

    #[test]
    fn create_maps_a_writable_segment() {
        let p = temp_path("create");
        let _ = std::fs::remove_file(&p);
        let m = FileMapping::create(&p, 64 * 1024).unwrap();
        assert_eq!(m.len(), 64 * 1024);
        vfs_ipc::ring::init(m.seg(), 4, 256).unwrap();
        drop(m);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn create_sizes_the_file_so_a_mapping_cannot_fault_on_touch() {
        // mmap beyond EOF succeeds and then SIGBUSes on access. `create` grows
        // the file to at least `size` precisely so that cannot happen.
        let p = temp_path("sized");
        let _ = std::fs::remove_file(&p);
        let m = FileMapping::create(&p, 64 * 1024).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 64 * 1024);
        drop(m);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn create_never_shrinks_an_existing_file() {
        // A second Director on the same ring path must not truncate the first
        // one's live mapping out from under it: MAP_SHARED over a shortened file
        // SIGBUSes on next touch rather than failing cleanly.
        let p = temp_path("noshrink");
        let _ = std::fs::remove_file(&p);
        let big = FileMapping::create(&p, 128 * 1024).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 128 * 1024);
        let small = FileMapping::create(&p, 64 * 1024).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().len(),
            128 * 1024,
            "create must grow-only; shrinking would SIGBUS the live mapping"
        );
        // The first mapping is still usable.
        // SAFETY: `big` maps 128 KiB of a file still at least that long.
        #[allow(unsafe_code)]
        unsafe {
            big.as_mut_ptr().add(100 * 1024).write_volatile(0xCD);
            assert_eq!(big.as_mut_ptr().add(100 * 1024).read_volatile(), 0xCD);
        }
        drop(small);
        drop(big);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn open_sees_the_ring_the_creator_wrote() {
        let p = temp_path("alias");
        let _ = std::fs::remove_file(&p);
        let creator = FileMapping::create(&p, 64 * 1024).unwrap();
        let geom_created = vfs_ipc::ring::init(creator.seg(), 4, 256).unwrap();
        let opener = FileMapping::open(&p, 64 * 1024).unwrap();
        let geom_opened = vfs_ipc::ring::open(opener.seg()).unwrap();
        assert_eq!(geom_created, geom_opened);
        drop(opener);
        drop(creator);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn writes_through_one_mapping_are_visible_through_another() {
        let p = temp_path("coherent");
        let _ = std::fs::remove_file(&p);
        let a = FileMapping::create(&p, 64 * 1024).unwrap();
        let b = FileMapping::open(&p, 64 * 1024).unwrap();
        // SAFETY: both map the same 64 KiB file; one byte at a fixed offset.
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
    fn open_missing_file_errors() {
        let p = temp_path("absent-xyz");
        let _ = std::fs::remove_file(&p);
        assert!(FileMapping::open(&p, 64 * 1024).is_err());
    }

    #[test]
    fn open_too_short_file_errors_rather_than_mapping_a_fault() {
        // A file shorter than `size` would map and then SIGBUS. Refuse it.
        let p = temp_path("short");
        let _ = std::fs::remove_file(&p);
        std::fs::write(&p, [0u8; 128]).unwrap();
        assert!(FileMapping::open(&p, 64 * 1024).is_err());
        assert_eq!(
            std::fs::metadata(&p).unwrap().len(),
            128,
            "a refused open must not have grown the file"
        );
        let _ = std::fs::remove_file(&p);
    }
}
