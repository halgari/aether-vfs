//! Userspace FUSE kernel: one provider per root, global file handles.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ops::{
    bad_request, is_dir, map_io_err, not_found, read_only, Access, DirEntry, Handle, Provider,
    RootId, SetAttr, Stat, VPath, OPEN_WRITE,
};
use crate::path::normalize;
use vfs_provider::OPEN_APPEND;

struct OpenRec {
    backend: Arc<dyn Provider>,
    bh: Handle,
    size: u64,
    is_dir: bool,
    /// Present, and equal to the file's size at open time, iff the handle
    /// was opened `OPEN_APPEND`. The director owns this cursor — providers
    /// stay purely positional — and a write on such a handle ignores the
    /// caller-supplied offset in favor of the cursor, advancing it by the
    /// number of bytes written.
    ///
    /// Known limitation: `Director::write` reads the cursor and writes it
    /// back under two separate lock acquisitions with the provider call
    /// unlocked in between, so two writes racing on *the same* `fh` — not
    /// just two distinct handles appending to the same file — can interleave
    /// incorrectly. Games write logs from a single handle used
    /// single-threaded, so this has not mattered; a per-path cursor (keyed
    /// by resolved provider + relative path rather than by `fh`), or holding
    /// the lock across the provider call, is the fix if it ever does.
    cursor: Option<u64>,
}

/// Userspace FUSE kernel. Maps each session root to exactly one provider and
/// hosts global file handles for getattr/open/read/write.
///
/// Resolution is a single map lookup, not a search: stage 2b task 3 deleted
/// the layer-ordered mount list and its reverse-iteration merge. Composition
/// across several sources — layering at the same path, or placing one at a
/// distinct sub-path within a root — now happens explicitly in the provider
/// graph *before* it reaches `mount` (see [`crate::mount_graph::MountGraph`]
/// and `vfs_compose::stack_layers`), where it is visible rather than
/// implicit here.
pub struct Director {
    roots: Mutex<BTreeMap<RootId, Arc<dyn Provider>>>,
    opens: Mutex<HashMap<u64, OpenRec>>,
    next_fh: AtomicU64,
}

impl Default for Director {
    fn default() -> Self {
        Self::new()
    }
}

impl Director {
    pub fn new() -> Self {
        Director {
            roots: Mutex::new(BTreeMap::new()),
            opens: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    /// Set (or replace) the single provider serving `root`. Composition of
    /// several sources into that one provider is the caller's job.
    pub fn mount(&self, root: RootId, backend: Arc<dyn Provider>) -> Result<(), i32> {
        self.roots
            .lock()
            .map_err(|_| map_io_err())?
            .insert(root, backend);
        Ok(())
    }

    /// Remove whatever provider serves `root`, if any (used when a session
    /// rebuilds that root's composition).
    pub fn unmount(&self, root: RootId) -> Result<(), i32> {
        self.roots.lock().map_err(|_| map_io_err())?.remove(&root);
        Ok(())
    }

    fn provider_for(&self, root: RootId) -> Result<Option<Arc<dyn Provider>>, i32> {
        Ok(self.roots.lock().map_err(|_| map_io_err())?.get(&root).cloned())
    }

    pub fn getattr(&self, root: RootId, path: &str) -> Result<Option<Stat>, i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        match self.provider_for(root)? {
            Some(p) => p.getattr(VPath::new(root, &path)),
            None => Ok(None),
        }
    }

    pub fn readdir(&self, root: RootId, path: &str) -> Result<Vec<DirEntry>, i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        match self.provider_for(root)? {
            Some(p) => p.readdir(VPath::new(root, &path)),
            None => Err(not_found()),
        }
    }

    /// Returns `(fh, size, is_dir)`.
    pub fn open(&self, root: RootId, path: &str, flags: u32) -> Result<(u64, u64, bool), i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let provider = self.provider_for(root)?.ok_or_else(not_found)?;
        if flags & OPEN_WRITE != 0 && provider.capabilities().access < Access::ReadWrite {
            // A configuration fact, not a caller mistake: this root has no
            // writable provider. Recorded by path so a later `vfs stats`
            // pass can surface it for discovery.
            crate::io_stats::record_rejected_write(&path);
            return Err(read_only());
        }
        let (bh, size, is_dir_flag) = provider.open(VPath::new(root, &path), flags)?;
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        let cursor = if flags & OPEN_APPEND != 0 { Some(size) } else { None };
        self.opens.lock().map_err(|_| map_io_err())?.insert(
            fh,
            OpenRec {
                backend: provider,
                bh,
                size,
                is_dir: is_dir_flag,
                cursor,
            },
        );
        Ok((fh, size, is_dir_flag))
    }

    pub fn read(&self, fh: u64, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (backend, bh, size, is_dir_flag) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let rec = g.get(&fh).ok_or_else(crate::ops::bad_fh)?;
            if rec.is_dir {
                return Err(is_dir());
            }
            (
                Arc::clone(&rec.backend),
                rec.bh,
                rec.size,
                rec.is_dir,
            )
        };
        let _ = (size, is_dir_flag);
        backend.read_at(bh, offset, buf)
    }

    pub fn close(&self, fh: u64) -> Result<(), i32> {
        let rec = {
            let mut g = self.opens.lock().map_err(|_| map_io_err())?;
            g.remove(&fh).ok_or_else(crate::ops::bad_fh)?
        };
        rec.backend.close(rec.bh)
    }

    /// Positional write, except on an append handle: there, the
    /// caller-supplied `offset` is ignored and the handle's own cursor is
    /// used instead, then advanced by the bytes actually written. See
    /// `OpenRec::cursor` for the caveat about two writes racing on the same
    /// handle.
    pub fn write(&self, fh: u64, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let (backend, bh, effective_offset) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let rec = g.get(&fh).ok_or_else(crate::ops::bad_fh)?;
            if rec.is_dir {
                return Err(is_dir());
            }
            (Arc::clone(&rec.backend), rec.bh, rec.cursor.unwrap_or(offset))
        };
        let result = backend.write_at(bh, effective_offset, buf);
        if let Ok(n) = result {
            if let Ok(mut g) = self.opens.lock() {
                if let Some(rec) = g.get_mut(&fh) {
                    if rec.cursor.is_some() {
                        rec.cursor = Some(effective_offset + n as u64);
                    }
                    rec.size = rec.size.max(effective_offset + n as u64);
                }
            }
        }
        result
    }

    /// Truncating or extending an append handle clamps its cursor to the new
    /// length rather than resetting it: a cursor already at or below `len`
    /// is still correct and must not jump forward, but one left past a
    /// shorter `len` would otherwise leave a hole on the next append.
    pub fn set_len(&self, fh: u64, len: u64) -> Result<(), i32> {
        let (backend, bh) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let rec = g.get(&fh).ok_or_else(crate::ops::bad_fh)?;
            (Arc::clone(&rec.backend), rec.bh)
        };
        let result = backend.set_len(bh, len);
        if result.is_ok() {
            if let Ok(mut g) = self.opens.lock() {
                if let Some(rec) = g.get_mut(&fh) {
                    rec.size = len;
                    if let Some(c) = rec.cursor.as_mut() {
                        *c = (*c).min(len);
                    }
                }
            }
        }
        result
    }

    pub fn flush(&self, fh: u64) -> Result<(), i32> {
        let (backend, bh) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let rec = g.get(&fh).ok_or_else(crate::ops::bad_fh)?;
            (Arc::clone(&rec.backend), rec.bh)
        };
        backend.flush(bh)
    }

    pub fn mkdir(&self, root: RootId, path: &str) -> Result<(), i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let provider = self.provider_for(root)?.ok_or_else(not_found)?;
        provider.mkdir(VPath::new(root, &path))
    }

    pub fn remove(&self, root: RootId, path: &str) -> Result<(), i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let provider = self.provider_for(root)?.ok_or_else(not_found)?;
        provider.remove(VPath::new(root, &path))
    }

    /// `from` and `to` are both resolved under `root` — a single root
    /// parameter makes a cross-root rename structurally impossible rather
    /// than a case to reject.
    pub fn rename(&self, root: RootId, from: &str, to: &str) -> Result<(), i32> {
        let from = normalize(from).map_err(|_| bad_request())?;
        let to = normalize(to).map_err(|_| bad_request())?;
        let provider = self.provider_for(root)?.ok_or_else(bad_request)?;
        provider.rename(VPath::new(root, &from), VPath::new(root, &to))
    }

    pub fn set_attr(&self, root: RootId, path: &str, attr: SetAttr) -> Result<(), i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let provider = self.provider_for(root)?.ok_or_else(not_found)?;
        provider.set_attr(VPath::new(root, &path), attr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::OPEN_READ;

    #[test]
    fn open_for_write_against_a_read_only_provider_is_read_only_not_bad_request() {
        // InlineProvider is Access::Read.
        let d = Director::new();
        d.mount(
            RootId::DEFAULT,
            Arc::new(vfs_compose::InlineProvider::from_files([("f", b"x".as_slice())])),
        )
        .unwrap();
        assert_eq!(
            d.open(RootId::DEFAULT, "f", OPEN_WRITE),
            Err(vfs_provider::ST_READ_ONLY)
        );
    }

    #[test]
    fn a_rejected_write_is_recorded_for_discovery() {
        let d = Director::new();
        d.mount(
            RootId::DEFAULT,
            Arc::new(vfs_compose::InlineProvider::from_files([("f", b"x".as_slice())])),
        )
        .unwrap();
        crate::io_stats::reset_rejected_writes();
        let _ = d.open(RootId::DEFAULT, "f", OPEN_WRITE);
        let rejected = crate::io_stats::rejected_writes();
        assert!(
            rejected.iter().any(|(path, count)| path == "f" && *count >= 1),
            "a rejected write must be discoverable, got {rejected:?}"
        );
    }

    #[test]
    fn write_then_read_through_the_director_round_trips() {
        let dir = std::env::temp_dir().join(format!("vfs-dirw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = Director::new();
        d.mount(RootId::DEFAULT, Arc::new(crate::DiskProvider::new(&dir))).unwrap();

        let (fh, _, _) = d
            .open(RootId::DEFAULT, "w.txt", OPEN_WRITE | vfs_provider::OPEN_CREATE)
            .unwrap();
        assert_eq!(d.write(fh, 0, b"hello").unwrap(), 5);
        d.close(fh).unwrap();

        let (fh, size, _) = d.open(RootId::DEFAULT, "w.txt", OPEN_READ).unwrap();
        assert_eq!(size, 5);
        let mut buf = [0u8; 8];
        let n = d.read(fh, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
        d.close(fh).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_handles_land_at_end_of_file() {
        let dir = std::env::temp_dir().join(format!("vfs-dira-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log.txt"), b"one").unwrap();
        let d = Director::new();
        d.mount(RootId::DEFAULT, Arc::new(crate::DiskProvider::new(&dir))).unwrap();

        let (fh, _, _) = d
            .open(RootId::DEFAULT, "log.txt", OPEN_WRITE | vfs_provider::OPEN_APPEND)
            .unwrap();
        // Offset 0 must be ignored on an append handle.
        d.write(fh, 0, b"two").unwrap();
        d.close(fh).unwrap();
        assert_eq!(std::fs::read(dir.join("log.txt")).unwrap(), b"onetwo");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_len_clamps_the_append_cursor_so_a_later_append_lands_at_the_new_end() {
        let dir = std::env::temp_dir().join(format!("vfs-dirsetlen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("log.txt"), b"0123456789").unwrap();
        let d = Director::new();
        d.mount(RootId::DEFAULT, Arc::new(crate::DiskProvider::new(&dir))).unwrap();

        let (fh, _, _) = d
            .open(RootId::DEFAULT, "log.txt", OPEN_WRITE | vfs_provider::OPEN_APPEND)
            .unwrap();
        // Cursor starts at 10 (the size at open). Truncating to 4 must clamp
        // it down too, or the next append would write at the stale offset
        // 10, leaving a hole between byte 4 and byte 10 instead of
        // continuing right after the new end.
        d.set_len(fh, 4).unwrap();
        assert_eq!(d.write(fh, 0, b"AB").unwrap(), 2);
        d.close(fh).unwrap();
        assert_eq!(
            std::fs::read(dir.join("log.txt")).unwrap(),
            b"0123AB",
            "append after a truncate must land at the new end with no hole"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_open_does_not_fall_through_a_read_only_top_mount_to_a_writable_one_beneath_it() {
        // A `MountGraph` with a writable `DiskProvider` mounted first (so it
        // resolves *underneath*) and a read-only `InlineProvider` mounted
        // second — making it the topmost resolved mount, per "later mounts
        // override earlier for the same path". A write open must fail with
        // `ST_READ_ONLY` at the top mount rather than silently falling
        // through and landing in the layer beneath — falling through risks a
        // write silently landing in an unintended (possibly immutable)
        // layer.
        let dir = std::env::temp_dir().join(format!("vfs-dirshadow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let graph = crate::mount_graph::MountGraph::new(vec![
            ("/".to_string(), Arc::new(crate::DiskProvider::new(&dir)) as Arc<dyn Provider>),
            (
                "/".to_string(),
                Arc::new(vfs_compose::InlineProvider::from_files([("f", b"x".as_slice())])),
            ),
        ])
        .unwrap();
        let d = Director::new();
        d.mount(RootId::DEFAULT, Arc::new(graph)).unwrap();

        assert_eq!(
            d.open(RootId::DEFAULT, "f", OPEN_WRITE),
            Err(vfs_provider::ST_READ_ONLY)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unmount_drops_visibility() {
        let dir = std::env::temp_dir().join(format!("vfs-unmount-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.txt"), b"x").unwrap();
        let d = Director::new();
        d.mount(RootId::DEFAULT, Arc::new(crate::DiskProvider::new(&dir))).unwrap();
        assert!(d.getattr(RootId::DEFAULT, "x.txt").unwrap().is_some());
        d.unmount(RootId::DEFAULT).unwrap();
        assert!(d.getattr(RootId::DEFAULT, "x.txt").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_roots_resolve_the_same_relative_path_independently() {
        // The direct-lookup counterpart to the ring-level test in
        // `ring_dispatch.rs`: `[0, "a.txt"]` and `[1, "a.txt"]` must reach
        // different providers.
        let d = Director::new();
        d.mount(
            RootId(0),
            Arc::new(vfs_compose::InlineProvider::from_files([("a.txt", b"ZERO".as_slice())])),
        )
        .unwrap();
        d.mount(
            RootId(1),
            Arc::new(vfs_compose::InlineProvider::from_files([("a.txt", b"ONE".as_slice())])),
        )
        .unwrap();

        let (fh0, size0, _) = d.open(RootId(0), "a.txt", OPEN_READ).unwrap();
        let mut buf0 = [0u8; 8];
        let n0 = d.read(fh0, 0, &mut buf0).unwrap();
        d.close(fh0).unwrap();

        let (fh1, size1, _) = d.open(RootId(1), "a.txt", OPEN_READ).unwrap();
        let mut buf1 = [0u8; 8];
        let n1 = d.read(fh1, 0, &mut buf1).unwrap();
        d.close(fh1).unwrap();

        assert_eq!(size0, 4);
        assert_eq!(&buf0[..n0], b"ZERO");
        assert_eq!(size1, 3);
        assert_eq!(&buf1[..n1], b"ONE");
    }
}
