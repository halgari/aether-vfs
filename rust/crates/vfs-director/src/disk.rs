//! Disk directory provider — maps a host folder under a mount.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::ops::{
    bad_request, map_io_err, not_a_dir, not_found, Access, Capabilities, DirEntry, Handle,
    Provider, SetAttr, Stat, VPath, KIND_DIR, KIND_FILE, OPEN_CREATE, OPEN_EXCL, OPEN_TRUNC,
    OPEN_WRITE,
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

    /// Map a `VPath::rel` onto a real path under `root`.
    ///
    /// `rel` is documented as arriving normalized (no `..`), but that is a
    /// contract on the caller, not a guarantee this provider can see
    /// enforced elsewhere — `vfs-source`'s gRPC boundary rejects a `..`
    /// component from a network client, but this is a different crate, and
    /// `open`'s new `OPEN_CREATE` handling escalates a containment slip from
    /// unauthorized read to unauthorized directory creation. Reject a bare
    /// `..` component here too, so containment does not depend solely on a
    /// caller in another crate getting it right. A filename that merely
    /// starts with `..` (e.g. `..foo`) is a normal path segment and passes
    /// through untouched.
    fn resolve(&self, path: &str) -> Result<PathBuf, i32> {
        if path.is_empty() {
            return Ok(self.root.clone());
        }
        let mut p = self.root.clone();
        for part in path.split('/') {
            if part.is_empty() {
                continue;
            }
            if part == ".." {
                return Err(bad_request());
            }
            p.push(part);
        }
        Ok(p)
    }
}

impl Provider for DiskProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::ReadWrite,
            immutable: false, // a real directory can change underneath us
            slow: false,
            preferred_block: None,
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let path = p.rel;
        let p = self.resolve(path)?;
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
        let p = self.resolve(path)?;
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
        let p = self.resolve(path)?;

        if flags & OPEN_WRITE == 0 {
            let meta = std::fs::metadata(&p).map_err(|_| not_found())?;
            if meta.is_dir() {
                let bh = self.next.fetch_add(1, Ordering::Relaxed);
                return Ok((bh, 0, true));
            }
            let f = File::open(&p).map_err(|_| map_io_err())?;
            let size = meta.len();
            let bh = self.next.fetch_add(1, Ordering::Relaxed);
            self.opens.lock().map_err(|_| map_io_err())?.insert(bh, f);
            return Ok((bh, size, false));
        }

        if flags & OPEN_CREATE != 0 {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).map_err(|_| map_io_err())?;
            }
        }

        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(flags & OPEN_CREATE != 0)
            .create_new(flags & OPEN_EXCL != 0)
            .truncate(flags & OPEN_TRUNC != 0)
            .open(&p)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    not_found()
                } else {
                    map_io_err()
                }
            })?;
        let size = f.metadata().map_err(|_| map_io_err())?.len();
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

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let mut g = self.opens.lock().map_err(|_| map_io_err())?;
        let f = g.get_mut(&h).ok_or_else(crate::ops::bad_fh)?;
        f.seek(SeekFrom::Start(offset)).map_err(|_| map_io_err())?;
        f.write(buf).map_err(|_| map_io_err())
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        let g = self.opens.lock().map_err(|_| map_io_err())?;
        let f = g.get(&h).ok_or_else(crate::ops::bad_fh)?;
        f.set_len(len).map_err(|_| map_io_err())
    }

    fn flush(&self, h: Handle) -> Result<(), i32> {
        let g = self.opens.lock().map_err(|_| map_io_err())?;
        let f = g.get(&h).ok_or_else(crate::ops::bad_fh)?;
        f.sync_all().map_err(|_| map_io_err())
    }

    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        let path = self.resolve(p.rel)?;
        std::fs::create_dir_all(&path).map_err(|_| map_io_err())
    }

    fn remove(&self, p: VPath) -> Result<(), i32> {
        let path = self.resolve(p.rel)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(not_found()),
            Err(_) => {
                // Windows reports "remove_file'd a directory" as plain
                // PermissionDenied — indistinguishable by ErrorKind from a
                // genuinely locked or permission-denied file. Consult
                // metadata to confirm this is actually a directory before
                // falling back, so a real file-removal failure is reported
                // as its own status instead of being replaced by whatever
                // remove_dir happens to return for a path that isn't one.
                if !std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false) {
                    return Err(map_io_err());
                }
                std::fs::remove_dir(&path).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        not_found()
                    } else {
                        map_io_err()
                    }
                })
            }
        }
    }

    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        if from.root != to.root {
            return Err(bad_request());
        }
        let from_path = self.resolve(from.rel)?;
        let to_path = self.resolve(to.rel)?;
        std::fs::rename(&from_path, &to_path).map_err(|_| map_io_err())
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        if attr.size.is_none() && attr.mtime.is_none() {
            return Ok(());
        }
        let path = self.resolve(p.rel)?;
        let f = File::options().write(true).open(&path).map_err(|_| map_io_err())?;
        if let Some(size) = attr.size {
            f.set_len(size).map_err(|_| map_io_err())?;
        }
        if let Some(mtime) = attr.mtime {
            let time = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(mtime.max(0) as u64);
            let times = std::fs::FileTimes::new().set_modified(time);
            f.set_times(times).map_err(|_| map_io_err())?;
        }
        Ok(())
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
    fn disk_provider_declares_read_write() {
        use vfs_provider::{Access, Provider};
        let dir = std::env::temp_dir().join(format!("vfs-diskrw-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);
        let p = DiskProvider::new(&dir);
        let caps = p.capabilities();
        assert_eq!(caps.access, Access::ReadWrite);
        assert!(!caps.immutable, "a real directory can change underneath us");
        caps.validate().expect("ReadWrite must not claim immutable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disk_provider_passes_write_conformance() {
        let dir = std::env::temp_dir().join(format!("vfs-diskwconf-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);
        let p: std::sync::Arc<dyn vfs_provider::Provider> = std::sync::Arc::new(DiskProvider::new(&dir));
        vfs_provider::assert_conformance(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_rejects_a_dotdot_component() {
        let dir = std::env::temp_dir().join(format!("vfs-diskdotdot-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);

        assert_eq!(
            DiskProvider::new(&dir).resolve("../escape.txt"),
            Err(bad_request()),
            "a leading .. component must be refused, not walked"
        );
        assert_eq!(
            DiskProvider::new(&dir).resolve("sub/../../escape.txt"),
            Err(bad_request()),
            "a .. component buried mid-path must be refused too"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_accepts_a_filename_that_merely_starts_with_dotdot() {
        let dir = std::env::temp_dir().join(format!("vfs-diskdotdotfoo-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);

        let resolved = DiskProvider::new(&dir)
            .resolve("..foo")
            .expect("..foo is a legitimate filename, not a .. component");
        assert_eq!(resolved, dir.join("..foo"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dotdot_escaping_open_is_refused_and_creates_nothing() {
        // Guards the OPEN_CREATE escalation directly: before this fix, an
        // OPEN_CREATE with a .. component would create_dir_all a directory
        // outside root. Confirm the parent of `dir` gains nothing.
        let parent = std::env::temp_dir();
        let dir = parent.join(format!("vfs-diskdotdotopen-{}", std::process::id()));
        vfs_provider::write_fixture_tree(&dir);
        let sibling = parent.join(format!("vfs-diskdotdotopen-{}-escaped", std::process::id()));
        let _ = std::fs::remove_dir_all(&sibling);

        use vfs_provider::{Provider, OPEN_CREATE, OPEN_WRITE};
        let p = DiskProvider::new(&dir);
        let escaping = format!(
            "../{}/pwned.txt",
            sibling.file_name().unwrap().to_string_lossy()
        );
        let result = p.open(VPath::at_default(&escaping), OPEN_WRITE | OPEN_CREATE);
        assert_eq!(result, Err(bad_request()));
        assert!(
            !sibling.exists(),
            "a .. escaping OPEN_CREATE must not create anything outside root"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&sibling);
    }
}
