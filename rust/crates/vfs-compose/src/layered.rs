//! Top-wins layering of two backends (Clojure `layered-provider`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_protocol::{
    bad_fh, map_io_err, not_found, Backend, BackendHandle, DirEntry, Stat, OPEN_WRITE,
};

enum Layer {
    Top,
    Bottom,
}

/// `top` shadows `bottom` on the same path; readdir unions with top-wins names.
pub struct LayeredBackend {
    top: Arc<dyn Backend>,
    bottom: Arc<dyn Backend>,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, (Layer, BackendHandle)>>,
}

impl LayeredBackend {
    pub fn new(top: Arc<dyn Backend>, bottom: Arc<dyn Backend>) -> Self {
        Self {
            top,
            bottom,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        }
    }

    fn routed(&self, layer: &Layer) -> &Arc<dyn Backend> {
        match layer {
            Layer::Top => &self.top,
            Layer::Bottom => &self.bottom,
        }
    }
}

impl Backend for LayeredBackend {
    fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
        match self.top.getattr(path)? {
            Some(s) => Ok(Some(s)),
            None => self.bottom.getattr(path),
        }
    }

    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let top_entries = match self.top.readdir(path) {
            Ok(e) => e,
            Err(e) if e == not_found() => {
                return self.bottom.readdir(path);
            }
            Err(e) => return Err(e),
        };
        let bottom_entries = match self.bottom.readdir(path) {
            Ok(e) => e,
            Err(_) => Vec::new(),
        };
        let mut seen: HashMap<String, DirEntry> = HashMap::new();
        // Bottom first, top overwrites.
        for e in bottom_entries {
            seen.insert(e.name.to_ascii_lowercase(), e);
        }
        for e in top_entries {
            seen.insert(e.name.to_ascii_lowercase(), e);
        }
        let mut out: Vec<DirEntry> = seen.into_values().collect();
        out.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
        Ok(out)
    }

    fn open(&self, path: &str, flags: u32) -> Result<(BackendHandle, u64, bool), i32> {
        if flags & OPEN_WRITE != 0 {
            // Read-only composition phase.
            return Err(vfs_protocol::bad_request());
        }
        let (layer, inner) = match self.top.open(path, flags) {
            Ok(r) => (Layer::Top, r),
            Err(e) if e == not_found() => {
                let r = self.bottom.open(path, flags)?;
                (Layer::Bottom, r)
            }
            Err(e) => return Err(e),
        };
        let (bh, size, is_dir) = inner;
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .insert(h, (layer, bh));
        Ok((h, size, is_dir))
    }

    fn read(&self, bh: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (layer, inner) = {
            let g = self.opens.lock().map_err(|_| map_io_err())?;
            let (l, i) = g.get(&bh).ok_or_else(bad_fh)?;
            (match l {
                Layer::Top => Layer::Top,
                Layer::Bottom => Layer::Bottom,
            }, *i)
        };
        self.routed(&layer).read(inner, offset, buf)
    }

    fn release(&self, bh: BackendHandle) -> Result<(), i32> {
        let (layer, inner) = {
            let mut g = self.opens.lock().map_err(|_| map_io_err())?;
            g.remove(&bh).ok_or_else(bad_fh)?
        };
        self.routed(&layer).release(inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InlineBackend;
    use vfs_protocol::{OPEN_READ, KIND_FILE};

    #[test]
    fn top_wins_shared_path() {
        let bottom = Arc::new(InlineBackend::from_files([
            ("shared.txt", b"FROM-BASE".as_slice()),
            ("meshes/base.nif", b"BASE".as_slice()),
        ]));
        let top = Arc::new(InlineBackend::from_files([
            ("shared.txt", b"MOD-WIN".as_slice()),
            ("textures/mod.dds", &[1u8; 10]),
        ]));
        let layered = LayeredBackend::new(top, bottom);

        let st = layered.getattr("shared.txt").unwrap().unwrap();
        assert_eq!(st.size, 7);
        assert_eq!(st.kind, KIND_FILE);

        let st = layered.getattr("meshes/base.nif").unwrap().unwrap();
        assert_eq!(st.size, 4);

        let st = layered.getattr("textures/mod.dds").unwrap().unwrap();
        assert_eq!(st.size, 10);

        let (h, size, _) = layered.open("shared.txt", OPEN_READ).unwrap();
        assert_eq!(size, 7);
        let mut buf = [0u8; 16];
        let n = layered.read(h, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"MOD-WIN");
        layered.release(h).unwrap();
    }

    #[test]
    fn readdir_unions_with_top_winning_names() {
        let bottom = Arc::new(InlineBackend::from_files([
            ("a.txt", b"A".as_slice()),
            ("shared.txt", b"BASE".as_slice()),
        ]));
        let top = Arc::new(InlineBackend::from_files([
            ("b.txt", b"B".as_slice()),
            ("shared.txt", b"TOP".as_slice()),
        ]));
        let layered = LayeredBackend::new(top, bottom);
        let entries = layered.readdir("").unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case("a.txt")));
        assert!(names.iter().any(|n| n.eq_ignore_ascii_case("b.txt")));
        let shared = entries
            .iter()
            .find(|e| e.name.eq_ignore_ascii_case("shared.txt"))
            .unwrap();
        assert_eq!(shared.stat.size, 3); // "TOP"
    }

    #[test]
    fn open_missing_on_top_falls_through() {
        let bottom = Arc::new(InlineBackend::from_files([("only-base.txt", b"BB".as_slice())]));
        let top = Arc::new(InlineBackend::from_files([("only-top.txt", b"TT".as_slice())]));
        let layered = LayeredBackend::new(top, bottom);
        let (h, size, _) = layered.open("only-base.txt", OPEN_READ).unwrap();
        assert_eq!(size, 2);
        let mut buf = [0u8; 4];
        assert_eq!(layered.read(h, 0, &mut buf).unwrap(), 2);
        layered.release(h).unwrap();
    }
}
