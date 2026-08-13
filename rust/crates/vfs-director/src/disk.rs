//! Disk directory backend — maps a host folder under a mount.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::ops::{
    bad_request, map_io_err, not_a_dir, not_found, Backend, BackendHandle, DirEntry, Stat,
    KIND_DIR, KIND_FILE, OPEN_WRITE,
};

pub struct DiskBackend {
    root: PathBuf,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, File>>,
}

impl DiskBackend {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DiskBackend {
            root: root.into(),
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        }
    }

    fn resolve(&self, path: &str) -> PathBuf {
        if path.is_empty() {
            self.root.clone()
        } else {
            let mut p = self.root.clone();
            for part in path.split('/') {
                if !part.is_empty() {
                    p.push(part);
                }
            }
            p
        }
    }
}

impl Backend for DiskBackend {
    fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
        let p = self.resolve(path);
        let meta = match std::fs::metadata(&p) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(map_io_err()),
        };
        if meta.is_dir() {
            Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }))
        } else if meta.is_file() {
            Ok(Some(Stat {
                kind: KIND_FILE,
                size: meta.len(),
                mtime: 0,
            }))
        } else {
            Ok(None)
        }
    }

    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let p = self.resolve(path);
        let rd = std::fs::read_dir(&p).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                not_found()
            } else if e.kind() == std::io::ErrorKind::NotADirectory {
                not_a_dir()
            } else {
                map_io_err()
            }
        })?;
        let mut out = Vec::new();
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().into_owned();
            let meta = ent.metadata().ok();
            let (kind, size) = match meta {
                Some(m) if m.is_dir() => (KIND_DIR, 0),
                Some(m) => (KIND_FILE, m.len()),
                None => continue,
            };
            out.push(DirEntry {
                name,
                stat: Stat {
                    kind,
                    size,
                    mtime: 0,
                },
            });
        }
        out.sort_by_key(|a| a.name.to_ascii_lowercase());
        Ok(out)
    }

    fn open(&self, path: &str, flags: u32) -> Result<(BackendHandle, u64, bool), i32> {
        if flags & OPEN_WRITE != 0 {
            return Err(bad_request());
        }
        let p = self.resolve(path);
        let meta = std::fs::metadata(&p).map_err(|_| not_found())?;
        if meta.is_dir() {
            let bh = self.next.fetch_add(1, Ordering::Relaxed);
            return Ok((bh, 0, true));
        }
        let f = File::open(&p).map_err(|_| map_io_err())?;
        let size = meta.len();
        let bh = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens.lock().map_err(|_| map_io_err())?.insert(bh, f);
        Ok((bh, size, false))
    }

    fn read(&self, bh: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let mut g = self.opens.lock().map_err(|_| map_io_err())?;
        let f = g.get_mut(&bh).ok_or_else(crate::ops::bad_fh)?;
        f.seek(SeekFrom::Start(offset)).map_err(|_| map_io_err())?;
        f.read(buf).map_err(|_| map_io_err())
    }

    fn release(&self, bh: BackendHandle) -> Result<(), i32> {
        let mut g = self.opens.lock().map_err(|_| map_io_err())?;
        // Dir opens may not be in the map.
        let _ = g.remove(&bh);
        Ok(())
    }
}


