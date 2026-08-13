//! Userspace FUSE kernel: mounts, overlay resolve, global file handles.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ops::{
    bad_fh, bad_request, is_dir, map_io_err, not_a_dir, not_found, read_only, Access, DirEntry,
    Handle, Provider, SetAttr, Stat, VPath, KIND_DIR, OPEN_WRITE,
};
use crate::path::{normalize, strip_prefix};
use vfs_provider::OPEN_APPEND;

struct Mount {
    prefix: String,
    backend: Arc<dyn Provider>,
}

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
    /// Known limitation: two handles appending to the same file
    /// concurrently can interleave incorrectly, since each tracks its own
    /// cursor rather than sharing one per path. Games write logs from a
    /// single handle, so this has not mattered; a per-path cursor (keyed by
    /// resolved mount + relative path rather than by `fh`) is the fix if it
    /// ever does.
    cursor: Option<u64>,
}

/// Userspace FUSE kernel. Hosts mount backends and call getattr/open/read.
pub struct Director {
    mounts: Mutex<Vec<Mount>>,
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
            mounts: Mutex::new(Vec::new()),
            opens: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
        }
    }

    /// Later mounts override earlier for the same path.
    pub fn mount(&self, prefix: &str, backend: Arc<dyn Provider>) -> Result<(), i32> {
        let prefix = normalize(prefix).map_err(|_| bad_request())?;
        self.mounts
            .lock()
            .map_err(|_| map_io_err())?
            .push(Mount { prefix, backend });
        Ok(())
    }

    /// Drop all mounts (used when a session rebuilds composition).
    pub fn clear_mounts(&self) -> Result<(), i32> {
        self.mounts
            .lock()
            .map_err(|_| map_io_err())?
            .clear();
        Ok(())
    }

    pub fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let mounts = self.mounts.lock().map_err(|_| map_io_err())?;
        for m in mounts.iter().rev() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            match m.backend.getattr(VPath::at_default(&rel))? {
                Some(s) => return Ok(Some(s)),
                None => continue,
            }
        }
        // Root always exists as a virtual dir if any mount is present.
        if path.is_empty() && !mounts.is_empty() {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        Ok(None)
    }

    pub fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let mounts = self.mounts.lock().map_err(|_| map_io_err())?;
        let mut map: HashMap<String, DirEntry> = HashMap::new();
        let mut saw_dir = false;
        let mut not_dir = false;
        for m in mounts.iter() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            match m.backend.readdir(VPath::at_default(&rel)) {
                Ok(entries) => {
                    saw_dir = true;
                    for e in entries {
                        map.insert(e.name.to_ascii_lowercase(), e);
                    }
                }
                Err(e) if e == not_found() => {}
                Err(e) if e == not_a_dir() => not_dir = true,
                Err(e) => return Err(e),
            }
        }
        if !saw_dir {
            if not_dir {
                return Err(not_a_dir());
            }
            // Empty virtual root
            if path.is_empty() && !mounts.is_empty() {
                return Ok(Vec::new());
            }
            return Err(not_found());
        }
        let mut out: Vec<DirEntry> = map.into_values().collect();
        out.sort_by_key(|a| a.name.to_ascii_lowercase());
        Ok(out)
    }

    /// Returns `(fh, size, is_dir)`.
    pub fn open(&self, path: &str, flags: u32) -> Result<(u64, u64, bool), i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let mounts = self.mounts.lock().map_err(|_| map_io_err())?;
        for m in mounts.iter().rev() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            if flags & OPEN_WRITE != 0 && m.backend.capabilities().access < Access::ReadWrite {
                // A configuration fact, not a caller mistake: this mount has
                // no writable provider behind it. Recorded by path so a
                // later `vfs stats` pass can surface it for discovery.
                crate::io_stats::record_rejected_write(&path);
                return Err(read_only());
            }
            match m.backend.open(VPath::at_default(&rel), flags) {
                Ok((bh, size, is_dir_flag)) => {
                    let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                    let cursor = if flags & OPEN_APPEND != 0 { Some(size) } else { None };
                    self.opens.lock().map_err(|_| map_io_err())?.insert(
                        fh,
                        OpenRec {
                            backend: Arc::clone(&m.backend),
                            bh,
                            size,
                            is_dir: is_dir_flag,
                            cursor,
                        },
                    );
                    return Ok((fh, size, is_dir_flag));
                }
                Err(e) if e == not_found() => continue,
                Err(e) => return Err(e),
            }
        }
        Err(not_found())
    }

    pub fn read(&self, fh: u64, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (backend, bh, size, is_dir_flag) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let rec = g.get(&fh).ok_or_else(bad_fh)?;
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
            g.remove(&fh).ok_or_else(bad_fh)?
        };
        rec.backend.close(rec.bh)
    }

    /// Positional write, except on an append handle: there, the
    /// caller-supplied `offset` is ignored and the handle's own cursor is
    /// used instead, then advanced by the bytes actually written. See
    /// `OpenRec::cursor` for the concurrent-appenders caveat.
    pub fn write(&self, fh: u64, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let (backend, bh, effective_offset) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let rec = g.get(&fh).ok_or_else(bad_fh)?;
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

    pub fn set_len(&self, fh: u64, len: u64) -> Result<(), i32> {
        let (backend, bh) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let rec = g.get(&fh).ok_or_else(bad_fh)?;
            (Arc::clone(&rec.backend), rec.bh)
        };
        let result = backend.set_len(bh, len);
        if result.is_ok() {
            if let Ok(mut g) = self.opens.lock() {
                if let Some(rec) = g.get_mut(&fh) {
                    rec.size = len;
                }
            }
        }
        result
    }

    pub fn flush(&self, fh: u64) -> Result<(), i32> {
        let (backend, bh) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let rec = g.get(&fh).ok_or_else(bad_fh)?;
            (Arc::clone(&rec.backend), rec.bh)
        };
        backend.flush(bh)
    }

    pub fn mkdir(&self, path: &str) -> Result<(), i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let mounts = self.mounts.lock().map_err(|_| map_io_err())?;
        for m in mounts.iter().rev() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            return m.backend.mkdir(VPath::at_default(&rel));
        }
        Err(not_found())
    }

    pub fn remove(&self, path: &str) -> Result<(), i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let mounts = self.mounts.lock().map_err(|_| map_io_err())?;
        for m in mounts.iter().rev() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            return m.backend.remove(VPath::at_default(&rel));
        }
        Err(not_found())
    }

    /// Both paths must resolve into the same mount; a rename that would
    /// cross mounts is rejected as a bad request rather than silently
    /// picking one side's mount.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), i32> {
        let from = normalize(from).map_err(|_| bad_request())?;
        let to = normalize(to).map_err(|_| bad_request())?;
        let mounts = self.mounts.lock().map_err(|_| map_io_err())?;
        for m in mounts.iter().rev() {
            let (Some(from_rel), Some(to_rel)) =
                (strip_prefix(&from, &m.prefix), strip_prefix(&to, &m.prefix))
            else {
                continue;
            };
            return m
                .backend
                .rename(VPath::at_default(&from_rel), VPath::at_default(&to_rel));
        }
        Err(bad_request())
    }

    pub fn set_attr(&self, path: &str, attr: SetAttr) -> Result<(), i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let mounts = self.mounts.lock().map_err(|_| map_io_err())?;
        for m in mounts.iter().rev() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            return m.backend.set_attr(VPath::at_default(&rel), attr);
        }
        Err(not_found())
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
        d.mount("/", Arc::new(vfs_compose::InlineProvider::from_files([("f", b"x".as_slice())])))
            .unwrap();
        assert_eq!(d.open("f", OPEN_WRITE), Err(vfs_provider::ST_READ_ONLY));
    }

    #[test]
    fn a_rejected_write_is_recorded_for_discovery() {
        let d = Director::new();
        d.mount("/", Arc::new(vfs_compose::InlineProvider::from_files([("f", b"x".as_slice())])))
            .unwrap();
        crate::io_stats::reset_rejected_writes();
        let _ = d.open("f", OPEN_WRITE);
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
        d.mount("/", Arc::new(crate::DiskProvider::new(&dir))).unwrap();

        let (fh, _, _) = d.open("w.txt", OPEN_WRITE | vfs_provider::OPEN_CREATE).unwrap();
        assert_eq!(d.write(fh, 0, b"hello").unwrap(), 5);
        d.close(fh).unwrap();

        let (fh, size, _) = d.open("w.txt", OPEN_READ).unwrap();
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
        d.mount("/", Arc::new(crate::DiskProvider::new(&dir))).unwrap();

        let (fh, _, _) = d.open("log.txt", OPEN_WRITE | vfs_provider::OPEN_APPEND).unwrap();
        // Offset 0 must be ignored on an append handle.
        d.write(fh, 0, b"two").unwrap();
        d.close(fh).unwrap();
        assert_eq!(std::fs::read(dir.join("log.txt")).unwrap(), b"onetwo");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
