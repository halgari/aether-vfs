//! The director **kernel**: one provider per root, a ring server the injected
//! shim talks to, and the leaf/staging pieces a host composes around them.
//!
//! **This is not the API a host embeds.** Session lifecycle — roots,
//! composition, serve, launch — lives in `vfs-embed` (design spec §4), which
//! is the one public seam `vfs.exe` and the language bindings are written
//! against. `Session` used to live here; it moved so that "the kernel" and
//! "the embeddable API" stopped being the same crate.
//!
//! What remains here, and what a host reaches for *through* `vfs-embed`:
//! * [`Director`] — the root → provider table and the handle namespace
//! * [`ipc::IpcServe`] — the shared-memory ring + workers
//! * [`DiskProvider`] / [`MountGraph`] — leaf and prefix-routing primitives
//! * [`stage`] — putting a launch image on real disk for `CreateProcess`
//! * [`io_stats`] — process-wide counters, including rejected writes
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
pub mod bench;
pub mod stage;

pub use director::Director;
pub use disk::DiskProvider;
pub use io_stats::{mark_launch as io_mark_launch, reset as io_stats_reset, snapshot_report as io_stats_report};
pub use mount_graph::MountGraph;
pub use ops::{Provider, Handle, DirEntry, RootId, Stat, KIND_DIR, KIND_FILE, OPEN_READ, OPEN_WRITE};
// Free-function form: `write_steam_appid` (skyrim-live.rs) writes the overlay
// copy before a `vfs_embed::Session` exists, so it needs this without an
// instance to call `Session::overlay_layer_dir` on — see that method's doc
// comment for why the path matters at all.
pub use vfs_provider::overlay_layer_dir;

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
}
