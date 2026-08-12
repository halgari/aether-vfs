//! In-memory file tree backend for tests (Clojure `inline-provider`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use vfs_protocol::{
    bad_fh, map_io_err, not_a_dir, not_found, Backend, BackendHandle, DirEntry, Stat, KIND_DIR,
    KIND_FILE, OPEN_WRITE,
};

struct FileData {
    bytes: Vec<u8>,
}

/// Flat map of virtual paths → file bytes. Parent dirs are synthesized.
pub struct InlineBackend {
    files: HashMap<String, FileData>,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, (String, Vec<u8>)>>,
}

impl InlineBackend {
    pub fn from_files<I, P, B>(entries: I) -> Self
    where
        I: IntoIterator<Item = (P, B)>,
        P: AsRef<str>,
        B: AsRef<[u8]>,
    {
        let mut files = HashMap::new();
        for (p, b) in entries {
            let path = normalize(p.as_ref());
            files.insert(
                path,
                FileData {
                    bytes: b.as_ref().to_vec(),
                },
            );
        }
        Self {
            files,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        }
    }
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/")
        .trim_matches('/')
        .to_string()
}

impl Backend for InlineBackend {
    fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
        let path = normalize(path);
        if path.is_empty() {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        if let Some(f) = self.files.get(&path) {
            return Ok(Some(Stat {
                kind: KIND_FILE,
                size: f.bytes.len() as u64,
                mtime: 0,
            }));
        }
        // Directory if any file has this prefix.
        let prefix = format!("{path}/");
        if self.files.keys().any(|k| k.starts_with(&prefix) || k == &path) {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        Ok(None)
    }

    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let path = normalize(path);
        if self.getattr(&path)?.map(|s| s.kind) != Some(KIND_DIR) {
            if self.files.contains_key(&path) {
                return Err(not_a_dir());
            }
            return Err(not_found());
        }
        let mut names: HashMap<String, Stat> = HashMap::new();
        for (k, f) in &self.files {
            let rel = if path.is_empty() {
                k.as_str()
            } else if let Some(rest) = k.strip_prefix(&format!("{path}/")) {
                rest
            } else {
                continue;
            };
            let name = rel.split('/').next().unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let is_file = !rel.contains('/');
            let st = if is_file {
                Stat {
                    kind: KIND_FILE,
                    size: f.bytes.len() as u64,
                    mtime: 0,
                }
            } else {
                Stat {
                    kind: KIND_DIR,
                    size: 0,
                    mtime: 0,
                }
            };
            names.entry(name.to_string()).or_insert(st);
        }
        Ok(names
            .into_iter()
            .map(|(name, stat)| DirEntry { name, stat })
            .collect())
    }

    fn open(&self, path: &str, flags: u32) -> Result<(BackendHandle, u64, bool), i32> {
        if flags & OPEN_WRITE != 0 {
            return Err(vfs_protocol::bad_request());
        }
        let path = normalize(path);
        let f = self.files.get(&path).ok_or_else(not_found)?;
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        let size = f.bytes.len() as u64;
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .insert(h, (path, f.bytes.clone()));
        Ok((h, size, false))
    }

    fn read(&self, bh: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let g = self.opens.lock().map_err(|_| map_io_err())?;
        let (_, bytes) = g.get(&bh).ok_or_else(bad_fh)?;
        if offset as usize >= bytes.len() {
            return Ok(0);
        }
        let start = offset as usize;
        let n = buf.len().min(bytes.len() - start);
        buf[..n].copy_from_slice(&bytes[start..start + n]);
        Ok(n)
    }

    fn release(&self, bh: BackendHandle) -> Result<(), i32> {
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .remove(&bh)
            .ok_or_else(bad_fh)?;
        Ok(())
    }
}
