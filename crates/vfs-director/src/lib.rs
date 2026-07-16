//! Userspace FUSE kernel: mount backends, overlay resolve, open table, C ABI.
//!
//! Zip and other stores implement [`Backend`]; they are **not** part of this crate.
//! Hosts create a [`Director`], mount backends (Rust or C ops), and call
//! getattr/readdir/open/read/close — or use the C exports in `ffi`.

#![deny(unsafe_code)]

pub mod director;
pub mod disk;
pub mod ops;
pub mod path;

#[allow(unsafe_code)]
pub mod ffi;

pub use director::Director;
pub use disk::DiskBackend;
pub use ops::{Backend, BackendHandle, DirEntry, Stat, KIND_DIR, KIND_FILE, OPEN_READ, OPEN_WRITE};

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
    fn overlay_later_mount_wins() {
        let a = std::env::temp_dir().join(format!("vfs-dir-a-{}", std::process::id()));
        let b = std::env::temp_dir().join(format!("vfs-dir-b-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&a);
        let _ = std::fs::create_dir_all(&b);
        std::fs::write(a.join("x.bin"), b"from-a").unwrap();
        std::fs::write(b.join("x.bin"), b"from-b").unwrap();
        let d = Director::new();
        d.mount("", Arc::new(DiskBackend::new(&a))).unwrap();
        d.mount("", Arc::new(DiskBackend::new(&b))).unwrap();
        let (fh, _, _) = d.open("x.bin", OPEN_READ).unwrap();
        let mut buf = [0u8; 8];
        let n = d.read(fh, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"from-b");
        d.close(fh).unwrap();
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }
}
