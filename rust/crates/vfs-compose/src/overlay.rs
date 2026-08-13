//! Minimal read-side overlay (whiteouts + upper-wins). Full CoW writes → later.
//!
//! Upper is a host directory: files present there win; `.wh.<name>` whiteouts
//! hide base entries. Base is any Provider (never mutated by this type's reads).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_provider::{
    bad_fh, bad_request, map_io_err, not_found, Capabilities, DirEntry, Handle, Provider, Stat,
    VPath, KIND_DIR, KIND_FILE, OPEN_WRITE,
};

enum Layer {
    Upper,
    Base,
}

/// Upper-over-base with `.wh.*` whiteouts (read path).
/// One open handle: which layer answered, the handle it gave back, and the
/// upper-layer file when the open was for writing.
type OpenEntry = (Layer, Handle, Option<std::fs::File>);

/// Upper-over-base with `.wh.*` whiteouts (read path). Stage 1 is read-only —
/// `capabilities` declares `Access::Read` and `open` rejects `OPEN_WRITE`; a
/// later stage promotes this to `ReadWrite` with copy-up.
pub struct OverlayProvider {
    base: Arc<dyn Provider>,
    upper: PathBuf,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, OpenEntry>>,
}

impl OverlayProvider {
    pub fn new(base: Arc<dyn Provider>, upper: impl Into<PathBuf>) -> Self {
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
        let parent = Path::new(vpath)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
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

impl Provider for OverlayProvider {
    fn capabilities(&self) -> Capabilities {
        // Stage 1 overlay only ever offers positional `read_at` (`open`
        // rejects OPEN_WRITE below, and there is no `read_next`), so access
        // must land on `Read` no matter what the base declares: `seekable()`
        // promotes a SeqRead base and `read_only_clamp()` demotes a
        // ReadWrite one. immutable/slow/preferred_block are real properties
        // of the base and must survive the wrap rather than being
        // hard-coded away.
        self.base.capabilities().seekable().read_only_clamp()
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let path = p.rel;
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
        self.base.getattr(p)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let path = p.rel;
        let mut map: HashMap<String, DirEntry> = HashMap::new();
        if let Ok(entries) = self.base.readdir(p) {
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
        } else if !path.is_empty() && self.getattr(p)?.is_none() {
            return Err(not_found());
        }
        let mut out: Vec<DirEntry> = map.into_values().collect();
        out.sort_by_key(|a| a.name.to_ascii_lowercase());
        Ok(out)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let path = p.rel;
        if flags & OPEN_WRITE != 0 {
            return Err(bad_request());
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
        let (bh, size, is_dir) = self.base.open(p, flags)?;
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .insert(h, (Layer::Base, bh, None));
        Ok((h, size, is_dir))
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        use std::io::{Read, Seek, SeekFrom};
        let mut g = self.opens.lock().map_err(|_| map_io_err())?;
        let (layer, inner, file) = g.get_mut(&h).ok_or_else(bad_fh)?;
        match layer {
            Layer::Upper => {
                let f = file.as_mut().ok_or_else(bad_fh)?;
                f.seek(SeekFrom::Start(offset)).map_err(|_| map_io_err())?;
                f.read(buf).map_err(|_| map_io_err())
            }
            Layer::Base => self.base.read_at(*inner, offset, buf),
        }
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let (layer, inner, _) = self
            .opens
            .lock()
            .map_err(|_| map_io_err())?
            .remove(&h)
            .ok_or_else(bad_fh)?;
        match layer {
            Layer::Upper => Ok(()),
            Layer::Base => self.base.close(inner),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InlineProvider;
    use vfs_provider::OPEN_READ;

    /// Slow and immutable, but sequential-only — exercises both the
    /// pass-through fields and the forced access clamp at once.
    struct SlowSeqBase;

    impl Provider for SlowSeqBase {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                access: vfs_provider::Access::SeqRead,
                immutable: true,
                slow: true,
                preferred_block: Some(4096),
            }
        }
        fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
            Ok(None)
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(Vec::new())
        }
        fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
            Err(not_found())
        }
        fn close(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }
        fn read_at(&self, _h: Handle, _o: u64, _b: &mut [u8]) -> Result<usize, i32> {
            Ok(0)
        }
    }

    #[test]
    fn overlay_capabilities_derive_from_base_but_clamp_access_to_read() {
        let dir = std::env::temp_dir().join(format!("vfs-ovcaps-{}", std::process::id()));
        let ov = OverlayProvider::new(Arc::new(SlowSeqBase), &dir);
        let caps = ov.capabilities();
        assert_eq!(
            caps.access,
            vfs_provider::Access::Read,
            "overlay only ever exposes positional read_at, regardless of the base's access"
        );
        assert!(caps.immutable, "immutable is a real property of the base");
        assert!(caps.slow, "slow is a real property of the base");
        assert_eq!(caps.preferred_block, Some(4096));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlay_over_the_fixture_tree_with_an_empty_upper_passes_conformance() {
        let base: Arc<dyn Provider> = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let dir = std::env::temp_dir().join(format!("vfs-ovconf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let p: Arc<dyn Provider> = Arc::new(OverlayProvider::new(base, &dir));
        vfs_provider::assert_conformance(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upper_wins_and_whiteout_hides() {
        let base = Arc::new(InlineProvider::from_files([
            ("a.txt", b"BASE".as_slice()),
            ("gone.txt", b"X".as_slice()),
        ]));
        let dir = std::env::temp_dir().join(format!("vfs-ov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"UPPER").unwrap();
        std::fs::write(dir.join(".wh.gone.txt"), b"").unwrap();
        let ov = OverlayProvider::new(base, &dir);

        let st = ov.getattr(VPath::at_default("a.txt")).unwrap().unwrap();
        assert_eq!(st.size, 5);
        assert!(ov.getattr(VPath::at_default("gone.txt")).unwrap().is_none());

        let (h, _, _) = ov.open(VPath::at_default("a.txt"), OPEN_READ).unwrap();
        let mut buf = [0u8; 8];
        let n = ov.read_at(h, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"UPPER");
        ov.close(h).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
