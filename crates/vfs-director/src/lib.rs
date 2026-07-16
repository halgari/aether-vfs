//! Userspace FUSE **host session**: mount backends, serve IPC, **launch** a
//! process with remapped I/O.
//!
//! Primary API for hosts:
//! 1. [`Session::new`] + path setters  
//! 2. [`Session::mount`] backends (zip/disk/C)  
//! 3. [`Session::serve`] — ring for the child shim  
//! 4. [`Session::launch`] — inject + remap  
//!
//! Host `open`/`read` exist for occasional inspection only.
//!
//! C ABI: see `include/vfs.h` (`vfs_director_*`, `vfs_launch`).
//!
//! Backend trait lives in [`vfs_ops`] so zip/inject do not form a crate cycle.

#![deny(unsafe_code)]

pub mod director;
pub mod disk;
pub mod ipc;
pub mod ops;
pub mod path;
pub mod ring_dispatch;
pub mod session;

#[allow(unsafe_code)]
pub mod ffi;

pub use director::Director;
pub use disk::DiskBackend;
pub use ops::{Backend, BackendHandle, DirEntry, Stat, KIND_DIR, KIND_FILE, OPEN_READ, OPEN_WRITE};
pub use session::{LaunchOpts, Session};

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;

    #[test]
    fn disk_backend_open_read() {
        let dir = std::env::temp_dir().join(format!("vfs-dir-disk-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("hello.txt");
        {
            let mut f = std::fs::File::create(&file).unwrap();
            f.write_all(b"hello-director").unwrap();
        }
        let d = Director::new();
        d.mount("", Arc::new(DiskBackend::new(&dir))).unwrap();
        let st = d.getattr("hello.txt").unwrap().unwrap();
        assert_eq!(st.kind, KIND_FILE);
        assert_eq!(st.size, 14);
        let (fh, size, is_dir) = d.open("hello.txt", OPEN_READ).unwrap();
        assert!(!is_dir && size == 14);
        let mut buf = [0u8; 32];
        let n = d.read(fh, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello-director");
        d.close(fh).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_read_file_helper() {
        let dir = std::env::temp_dir().join(format!("vfs-sess-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("a.bin"), b"xyz").unwrap();
        let mut s = Session::new();
        s.mount("", Arc::new(DiskBackend::new(&dir))).unwrap();
        let got = s.read_file("a.bin").unwrap();
        assert_eq!(got, b"xyz");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
