//! Zip as a userspace FUSE **backend** — no vfs-core Layer/Source types.
//!
//! Implements [`vfs_protocol::Backend`] for Stored entries only. Path lookups are
//! **case-insensitive** (Windows game paths).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use vfs_protocol::{
    Backend, BackendHandle, DirEntry, Stat, KIND_DIR, KIND_FILE, OPEN_WRITE,
};
use vfs_protocol::{ST_BAD_FH, ST_BAD_REQUEST, ST_IO_ERROR, ST_NOT_A_DIRECTORY, ST_NOT_FOUND};

use crate::{data_offset, locate_central_directory, read_central_directory, ZipError};

#[derive(Clone)]
struct Node {
    is_dir: bool,
    size: u64,
    mtime: i64,
    /// Absolute offset of file payload in the zip container.
    data_off: u64,
}

struct Live {
    file: File,
    base: u64,
    size: u64,
}

/// Zip archive mounted as a backend (path index + open handles).
pub struct ZipBackend {
    container: PathBuf,
    /// Canonical path (as in the zip CD) → node.
    nodes: HashMap<String, Node>,
    /// ASCII-lowercase path → canonical key (Windows-style).
    by_fold: HashMap<String, String>,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, Live>>,
}

impl ZipBackend {
    pub fn open(zip_path: &Path) -> Result<Self, ZipError> {
        let mut f = File::open(zip_path)?;
        let file_len = f.metadata()?.len();
        let (cd_off, cd_count) = locate_central_directory(&mut f, file_len)?;
        let entries = read_central_directory(&mut f, cd_off, cd_count)?;

        let mut nodes: HashMap<String, Node> = HashMap::new();
        for e in entries {
            if e.name.ends_with('/') {
                let vpath = e.name.trim_end_matches('/').to_string();
                ensure_parents(&mut nodes, &vpath);
                nodes.insert(
                    vpath,
                    Node {
                        is_dir: true,
                        size: 0,
                        mtime: e.mtime,
                        data_off: 0,
                    },
                );
                continue;
            }
            if e.method != 0 {
                return Err(ZipError::Unsupported(format!(
                    "entry {} uses compression method {} (only Stored is supported)",
                    e.name, e.method
                )));
            }
            let data_off = data_offset(&mut f, e.local_header_off)?;
            let vpath = e.name.trim_start_matches('/').replace('\\', "/");
            ensure_parents(&mut nodes, &vpath);
            nodes.insert(
                vpath,
                Node {
                    is_dir: false,
                    size: e.uncomp_size,
                    mtime: e.mtime,
                    data_off,
                },
            );
        }

        let mut by_fold = HashMap::with_capacity(nodes.len());
        for key in nodes.keys() {
            by_fold.insert(key.to_ascii_lowercase(), key.clone());
        }

        Ok(ZipBackend {
            container: zip_path.to_path_buf(),
            nodes,
            by_fold,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
        })
    }

    fn get(&self, path: &str) -> Option<&Node> {
        let p = path.trim_start_matches('/');
        if let Some(n) = self.nodes.get(p) {
            return Some(n);
        }
        let canon = self.by_fold.get(&p.to_ascii_lowercase())?;
        self.nodes.get(canon)
    }
}

fn ensure_parents(nodes: &mut HashMap<String, Node>, vpath: &str) {
    // Only intermediate directories (exclude the leaf — caller inserts the leaf).
    let Some((parent, _)) = vpath.rsplit_once('/') else {
        return;
    };
    let mut acc = String::new();
    for part in parent.split('/') {
        if part.is_empty() {
            continue;
        }
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(part);
        nodes.entry(acc.clone()).or_insert(Node {
            is_dir: true,
            size: 0,
            mtime: 0,
            data_off: 0,
        });
    }
}

impl Backend for ZipBackend {
    fn getattr(&self, path: &str) -> Result<Option<Stat>, i32> {
        let p = path.trim_start_matches('/');
        if p.is_empty() {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        Ok(self.get(p).map(|n| Stat {
            kind: if n.is_dir { KIND_DIR } else { KIND_FILE },
            size: n.size,
            mtime: n.mtime,
        }))
    }

    fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, i32> {
        let p = path.trim_start_matches('/');
        let canon = if p.is_empty() {
            String::new()
        } else {
            match self.get(p) {
                Some(n) if n.is_dir => self
                    .by_fold
                    .get(&p.to_ascii_lowercase())
                    .cloned()
                    .unwrap_or_else(|| p.to_string()),
                Some(_) => return Err(ST_NOT_A_DIRECTORY),
                None => return Err(ST_NOT_FOUND),
            }
        };
        let prefix = if canon.is_empty() {
            String::new()
        } else {
            format!("{canon}/")
        };
        let mut kids: HashMap<String, DirEntry> = HashMap::new();
        for (name, node) in &self.nodes {
            let rest = if prefix.is_empty() {
                name.as_str()
            } else if let Some(r) = name.strip_prefix(&prefix) {
                r
            } else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            // Only immediate children.
            if rest.contains('/') {
                let child = rest.split('/').next().unwrap();
                kids.entry(child.to_string()).or_insert(DirEntry {
                    name: child.to_string(),
                    stat: Stat {
                        kind: KIND_DIR,
                        size: 0,
                        mtime: 0,
                    },
                });
            } else {
                kids.insert(
                    rest.to_string(),
                    DirEntry {
                        name: rest.to_string(),
                        stat: Stat {
                            kind: if node.is_dir { KIND_DIR } else { KIND_FILE },
                            size: node.size,
                            mtime: node.mtime,
                        },
                    },
                );
            }
        }
        let mut out: Vec<DirEntry> = kids.into_values().collect();
        out.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
        Ok(out)
    }

    fn open(&self, path: &str, flags: u32) -> Result<(BackendHandle, u64, bool), i32> {
        if flags & OPEN_WRITE != 0 {
            return Err(ST_BAD_REQUEST);
        }
        let p = path.trim_start_matches('/');
        if p.is_empty() {
            let bh = self.next.fetch_add(1, Ordering::Relaxed);
            return Ok((bh, 0, true));
        }
        let node = self.get(p).ok_or(ST_NOT_FOUND)?;
        if node.is_dir {
            let bh = self.next.fetch_add(1, Ordering::Relaxed);
            return Ok((bh, 0, true));
        }
        let file = File::open(&self.container).map_err(|_| ST_IO_ERROR)?;
        let bh = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens
            .lock()
            .map_err(|_| ST_IO_ERROR)?
            .insert(
                bh,
                Live {
                    file,
                    base: node.data_off,
                    size: node.size,
                },
            );
        Ok((bh, node.size, false))
    }

    fn read(&self, bh: BackendHandle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let mut g = self.opens.lock().map_err(|_| ST_IO_ERROR)?;
        let live = g.get_mut(&bh).ok_or(ST_BAD_FH)?;
        if offset >= live.size {
            return Ok(0);
        }
        let max = ((live.size - offset) as usize).min(buf.len());
        let abs = live.base + offset;
        live.file
            .seek(SeekFrom::Start(abs))
            .map_err(|_| ST_IO_ERROR)?;
        live.file.read(&mut buf[..max]).map_err(|_| ST_IO_ERROR)
    }

    fn release(&self, bh: BackendHandle) -> Result<(), i32> {
        let mut g = self.opens.lock().map_err(|_| ST_IO_ERROR)?;
        let _ = g.remove(&bh);
        Ok(())
    }
}
