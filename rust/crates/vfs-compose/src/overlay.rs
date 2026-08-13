//! Minimal read-side overlay (whiteouts + upper-wins). Full CoW writes → later.
//!
//! Upper is a host directory: files present there win; `.wh.<name>` whiteouts
//! hide base entries. Base is any Backend (never mutated by this type's reads).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_protocol::{
    bad_fh, map_io_err, not_found, Backend, BackendHandle, DirEntry, Stat, KIND_DIR, KIND_FILE,
    OPEN_WRITE,
};

enum Layer {
    Upper,
    Base,
}

/// Upper-over-base with `.wh.*` whiteouts (read path).
/// One open handle: which layer answered, the handle it gave back, and the
/// upper-layer file when the open was for writing.
type OpenEntry = (Layer, BackendHandle, Option<std::fs::File>);

pub struct OverlayBackend {
    base: Arc<dyn Backend>,
    upper: PathBuf,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, OpenEntry>>,
}

impl OverlayBackend {
    pub fn new(base: Arc<dyn Backend>, upper: impl Into<PathBuf>) -> Self {
        let upper = upper.into();
        let _ = std::fs::create_dir_all(&upper);
        Self {
            base,
            upper,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        }
    }

    fn upper_path(&self, vpath: &str) -> PathBuf {
        let mut p = self.upper.clone();
        for part in vpath.split('/').filter(|s| !s.is_empty()) {
            p.push(part);
        }
        p
    }

    fn whiteout_path(&self, vpath: &str) -> PathBuf {
        let parent = Path::new(vpath).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let name = Path::new(vpath)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let wh = format!(".wh.{name}");
        if parent.is_empty() {
            self.upper.join(wh)
        } else {
            self.upper_path(&parent).join(wh)
        }
    }

    fn is_whiteout(&self, vpath: &str) -> bool {
        self.whiteout_path(vpath).is_file()
    }
}

impl Backend for OverlayBackend {
    fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
        if path.is_empty() {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        if self.is_whiteout(path) {
            return Ok(None);
        }
        let up = self.upper_path(path);
        if up.is_file() {
            let meta = std::fs::metadata(&up).map_err(|_| map_io_err())?;
            return Ok(Some(Stat {
                kind: KIND_FILE,
                size: meta.len(),
                mtime: 0,
            }));
        }
        if up.is_dir() {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        self.base.getattr(path)
    }

    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let mut map: HashMap<String, DirEntry> = HashMap::new();
        if let Ok(entries) = self.base.readdir(path) {
            for e in entries {
                map.insert(e.name.to_ascii_lowercase(), e);
            }
        }
        let up = self.upper_path(path);
        if up.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&up) {
                for ent in rd.flatten() {
                    let name = ent.file_name().to_string_lossy().into_owned();
                    if let Some(target) = name.strip_prefix(".wh.") {
                        map.remove(&target.to_ascii_lowercase());
                        continue;
                    }
                    let meta = ent.metadata().ok();
                    let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
                    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                    map.insert(
                        name.to_ascii_lowercase(),
                        DirEntry {
                            name,
                            stat: Stat {
                                kind: if is_dir { KIND_DIR } else { KIND_FILE },
                                size,
                                mtime: 0,
                            },
                        },
                    );
                }
            }
        } else if !path.is_empty() && self.getattr(path)?.is_none() {
            return Err(not_found());
        }
        let mut out: Vec<DirEntry> = map.into_values().collect();
        out.sort_by_key(|a| a.name.to_ascii_lowercase());
        Ok(out)
    }

    fn open(&self, path: &str, flags: u32) -> Result<(BackendHandle, u64, bool), i32> {
        if flags & OPEN_WRITE != 0 {
            return Err(vfs_protocol::bad_request());
        }
        if self.is_whiteout(path) {
            return Err(not_found());
        }
        let up = self.upper_path(path);
        if up.is_file() {
            let f = std::fs::File::open(&up).map_err(|_| map_io_err())?;
            let size = f.metadata().map(|m| m.len()).unwrap_or(0);
            let h = self.next.fetch_add(1, Ordering::Relaxed);
            self.opens
                .lock()
                .map_err(|_| map_io_err())?
                .insert(h, (Layer::Upper, 0, Some(f)));
            return Ok((h, size, false));
        }
        let (bh, size, is_dir) = self.base.open(path, flags)?;
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .insert(h, (Layer::Base, bh, None));
        Ok((h, size, is_dir))
    }

    fn read(&self, bh: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        use std::io::{Read, Seek, SeekFrom};
        let mut g = self.opens.lock().map_err(|_| map_io_err())?;
        let (layer, inner, file) = g.get_mut(&bh).ok_or_else(bad_fh)?;
        match layer {
            Layer::Upper => {
                let f = file.as_mut().ok_or_else(bad_fh)?;
                f.seek(SeekFrom::Start(offset)).map_err(|_| map_io_err())?;
                f.read(buf).map_err(|_| map_io_err())
            }
            Layer::Base => self.base.read(*inner, offset, buf),
        }
    }

    fn release(&self, bh: BackendHandle) -> Result<(), i32> {
        let (layer, inner, _) = self
            .opens
            .lock()
            .map_err(|_| map_io_err())?
            .remove(&bh)
            .ok_or_else(bad_fh)?;
        match layer {
            Layer::Upper => Ok(()),
            Layer::Base => self.base.release(inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InlineBackend;
    use vfs_protocol::OPEN_READ;

    #[test]
    fn upper_wins_and_whiteout_hides() {
        let base = Arc::new(InlineBackend::from_files([
            ("a.txt", b"BASE".as_slice()),
            ("gone.txt", b"X".as_slice()),
        ]));
        let dir = std::env::temp_dir().join(format!("vfs-ov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"UPPER").unwrap();
        std::fs::write(dir.join(".wh.gone.txt"), b"").unwrap();
        let ov = OverlayBackend::new(base, &dir);

        let st = ov.getattr("a.txt").unwrap().unwrap();
        assert_eq!(st.size, 5);
        assert!(ov.getattr("gone.txt").unwrap().is_none());

        let (h, _, _) = ov.open("a.txt", OPEN_READ).unwrap();
        let mut buf = [0u8; 8];
        let n = ov.read(h, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"UPPER");
        ov.release(h).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
