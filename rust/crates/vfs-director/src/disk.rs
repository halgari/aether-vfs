//! Disk directory provider — maps a host folder under a mount.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::ops::{
    bad_request, map_io_err, not_a_dir, not_found, Access, Capabilities, DirEntry, Handle,
    Provider, Stat, VPath, KIND_DIR, KIND_FILE, OPEN_WRITE,
};

pub struct DiskProvider {
    root: PathBuf,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, File>>,
}

impl DiskProvider {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        DiskProvider {
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

impl Provider for DiskProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::Read, // writes arrive in Stage 3
            immutable: false,     // a real directory can change underneath us
            slow: false,
            preferred_block: None,
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let path = p.rel;
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

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let path = p.rel;
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

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let path = p.rel;
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

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let mut g = self.opens.lock().map_err(|_| map_io_err())?;
        let f = g.get_mut(&h).ok_or_else(crate::ops::bad_fh)?;
        f.seek(SeekFrom::Start(offset)).map_err(|_| map_io_err())?;
        f.read(buf).map_err(|_| map_io_err())
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let mut g = self.opens.lock().map_err(|_| map_io_err())?;
        // Dir opens may not be in the map.
        let _ = g.remove(&h);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_provider_declares_mutable_read_access() {
        use vfs_provider::{Access, Provider};
        let dir = std::env::temp_dir().join(format!("vfs-diskcaps-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);

        let p = DiskProvider::new(&dir);
        let caps = p.capabilities();
        assert_eq!(caps.access, Access::Read, "writes arrive in Stage 3");
        assert!(!caps.immutable, "a real directory can change underneath us");
        caps.validate().expect("declaration must be self-consistent");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_provider_passes_conformance() {
        let dir = std::env::temp_dir().join(format!("vfs-diskconf-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);

        let p: std::sync::Arc<dyn vfs_provider::Provider> = std::sync::Arc::new(DiskProvider::new(&dir));
        vfs_provider::assert_conformance(p);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
