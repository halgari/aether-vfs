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
//! `Provider` trait lives in [`vfs_protocol`] (ops module, re-exported from
//! `vfs-provider`) so zip stays free of host deps.

#![deny(unsafe_code)]

pub mod director;
pub mod disk;
pub mod io_stats;
pub mod ipc;
pub mod mount_graph;
pub mod ops;
pub mod path;
pub mod ring_dispatch;
pub mod session;
pub mod bench;
pub mod stage;

pub use director::Director;
pub use disk::DiskProvider;
pub use io_stats::{mark_launch as io_mark_launch, reset as io_stats_reset, snapshot_report as io_stats_report};
pub use mount_graph::MountGraph;
pub use ops::{Provider, Handle, DirEntry, RootId, Stat, KIND_DIR, KIND_FILE, OPEN_READ, OPEN_WRITE};
pub use session::{LaunchOpts, Session};

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;

    #[test]
    fn disk_provider_open_read() {
        let dir = std::env::temp_dir().join(format!("vfs-dir-disk-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("hello.txt");
        {
            let mut f = std::fs::File::create(&file).unwrap();
            f.write_all(b"hello-director").unwrap();
        }
        let d = Director::new();
        d.mount(RootId::DEFAULT, Arc::new(DiskProvider::new(&dir))).unwrap();
        let st = d.getattr(RootId::DEFAULT, "hello.txt").unwrap().unwrap();
        assert_eq!(st.kind, KIND_FILE);
        assert_eq!(st.size, 14);
        let (fh, size, is_dir) = d.open(RootId::DEFAULT, "hello.txt", OPEN_READ).unwrap();
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
        let s = Session::new();
        s.mount("", Arc::new(DiskProvider::new(&dir))).unwrap();
        let got = s.read_file("a.bin").unwrap();
        assert_eq!(got, b"xyz");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // Named distinctly from `director::tests::unmount_drops_visibility` —
    // this crate-root smoke test and that one exercised the identical
    // behavior under the identical leaf name after both were independently
    // renamed from `clear_mounts_drops_visibility`, which is exactly the
    // kind of thing that makes a test count silently miscounted by one.
    #[test]
    fn bare_director_unmount_drops_visibility() {
        let dir = std::env::temp_dir().join(format!("vfs-clear-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("x.txt"), b"x").unwrap();
        let d = Director::new();
        d.mount(RootId::DEFAULT, Arc::new(DiskProvider::new(&dir))).unwrap();
        assert!(d.getattr(RootId::DEFAULT, "x.txt").unwrap().is_some());
        d.unmount(RootId::DEFAULT).unwrap();
        assert!(d.getattr(RootId::DEFAULT, "x.txt").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_serve_and_ring_read() {
        use vfs_protocol::{
            decode_open_resp, decode_read_resp, encode_open_req, encode_read_req, OpenResp, ReadReq,
            OP_OPEN, OP_READ, OPEN_READ, ST_OK,
        };

        let dir = std::env::temp_dir().join(format!("vfs-sess-ring-{}", std::process::id()));
        let state = std::env::temp_dir().join(format!("vfs-sess-st-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("payload.bin"), b"ring-bytes").unwrap();

        let mut s = Session::new();
        s.set_root(&dir);
        s.set_state_dir(&state);
        s.mount("", Arc::new(DiskProvider::new(&dir))).unwrap();
        s.serve().expect("serve");
        assert!(s.is_serving());

        {
            let ipc = s.ipc().expect("ipc");
            let client = ipc.client().expect("client");
            let open = client
                .submit(OP_OPEN, 0, &encode_open_req(0, OPEN_READ, "payload.bin"))
                .unwrap();
            assert_eq!(open.status, ST_OK);
            let OpenResp { fh, size, .. } = decode_open_resp(&open.payload).unwrap();
            assert_eq!(size, 10);
            let r = client
                .submit(
                    OP_READ,
                    0,
                    &encode_read_req(&ReadReq {
                        fh,
                        offset: 0,
                        len: 10,
                    }),
                )
                .unwrap();
            assert_eq!(r.status, ST_OK);
            assert_eq!(decode_read_resp(&r.payload).unwrap(), b"ring-bytes");
        }

        s.stop_serve();
        assert!(!s.is_serving());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state);
    }
}
