//! Userspace FUSE kernel: mounts, overlay resolve, global file handles.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::ops::{
    bad_fh, bad_request, is_dir, map_io_err, not_a_dir, not_found, Backend, BackendHandle, DirEntry,
    Stat, KIND_DIR, KIND_FILE, OPEN_WRITE,
};
use crate::path::{normalize, strip_prefix};

struct Mount {
    prefix: String,
    backend: Arc<dyn Backend>,
}

struct OpenRec {
    backend: Arc<dyn Backend>,
    bh: BackendHandle,
    size: u64,
    is_dir: bool,
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
    pub fn mount(&self, prefix: &str, backend: Arc<dyn Backend>) -> Result<(), i32> {
        let prefix = normalize(prefix).map_err(|_| bad_request())?;
        self.mounts
            .lock()
            .map_err(|_| map_io_err())?
            .push(Mount { prefix, backend });
        Ok(())
    }

    pub fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
        let path = normalize(path).map_err(|_| bad_request())?;
        let mounts = self.mounts.lock().map_err(|_| map_io_err())?;
        for m in mounts.iter().rev() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            match m.backend.getattr(&rel)? {
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
            match m.backend.readdir(&rel) {
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
        out.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
        Ok(out)
    }

    /// Returns `(fh, size, is_dir)`.
    pub fn open(&self, path: &str, flags: u32) -> Result<(u64, u64, bool), i32> {
        if flags & OPEN_WRITE != 0 {
            return Err(bad_request());
        }
        let path = normalize(path).map_err(|_| bad_request())?;
        let mounts = self.mounts.lock().map_err(|_| map_io_err())?;
        for m in mounts.iter().rev() {
            let Some(rel) = strip_prefix(&path, &m.prefix) else {
                continue;
            };
            match m.backend.open(&rel, flags) {
                Ok((bh, size, is_dir_flag)) => {
                    let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                    self.opens.lock().map_err(|_| map_io_err())?.insert(
                        fh,
                        OpenRec {
                            backend: Arc::clone(&m.backend),
                            bh,
                            size,
                            is_dir: is_dir_flag,
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
        backend.read(bh, offset, buf)
    }

    pub fn close(&self, fh: u64) -> Result<(), i32> {
        let rec = {
            let mut g = self.opens.lock().map_err(|_| map_io_err())?;
            g.remove(&fh).ok_or_else(bad_fh)?
        };
        rec.backend.release(rec.bh)
    }

    /// Helper matching ring AttrResp-style checks.
    pub fn is_file(&self, path: &str) -> Result<bool, i32> {
        match self.getattr(path)? {
            Some(s) if s.kind == KIND_FILE => Ok(true),
            Some(s) if s.kind == KIND_DIR => Ok(false),
            _ => Ok(false),
        }
    }
}
