//! Zip as a userspace FUSE **provider** — no vfs-core Layer/Source types.
//!
//! Implements [`vfs_provider::Provider`] for Stored entries only. Path lookups are
//! **case-insensitive** (Windows game paths).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use vfs_core::fold;
use vfs_provider::{
    Access, Capabilities, CaseMatch, DirEntry, Handle, Provider, Stat, VPath, KIND_DIR, KIND_FILE,
    OPEN_WRITE,
};
use vfs_provider::{ST_BAD_FH, ST_BAD_REQUEST, ST_IO_ERROR, ST_NOT_A_DIRECTORY, ST_NOT_FOUND};

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

/// Zip archive mounted as a provider (path index + open handles).
pub struct ZipProvider {
    container: PathBuf,
    /// Canonical path (as in the zip CD) → node.
    nodes: HashMap<String, Node>,
    /// Folded path → canonical key (Windows-style). Folded with
    /// [`vfs_core::fold`], the same function the shim folds vpath components
    /// with before they cross the ring — an ASCII-only fold here would miss
    /// every entry whose case only Unicode knows how to lower.
    by_fold: HashMap<String, String>,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, Live>>,
}

impl ZipProvider {
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
            by_fold.insert(fold(key), key.clone());
        }

        Ok(ZipProvider {
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
        let canon = self.by_fold.get(&fold(p))?;
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

impl Provider for ZipProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::Read,
            immutable: true,
            slow: false,
            preferred_block: None,
            // Path lookups fold (see the module doc and `by_fold` below).
            case: CaseMatch::Insensitive,
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let path = p.rel;
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

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let path = p.rel;
        let p = path.trim_start_matches('/');
        let canon = if p.is_empty() {
            String::new()
        } else {
            match self.get(p) {
                Some(n) if n.is_dir => self
                    .by_fold
                    .get(&fold(p))
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
        out.sort_by_key(|a| fold(&a.name));
        Ok(out)
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        if flags & OPEN_WRITE != 0 {
            return Err(ST_BAD_REQUEST);
        }
        let path = p.rel;
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

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        // Serialize seek+read on the live File. Concurrent try_clone + seek from
        // multiple director workers was part of the post-seal 0xC0000409 regression
        // (corrupted / racy BSA streams). Revisit with per-handle File handles later.
        let mut g = self.opens.lock().map_err(|_| ST_IO_ERROR)?;
        let live = g.get_mut(&h).ok_or(ST_BAD_FH)?;
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

    fn close(&self, h: Handle) -> Result<(), i32> {
        let mut g = self.opens.lock().map_err(|_| ST_IO_ERROR)?;
        let _ = g.remove(&h);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const LFH_SIG: u32 = 0x0403_4b50;
    const CDH_SIG: u32 = 0x0201_4b50;
    const EOCD_SIG: u32 = 0x0605_4b50;

    /// Tiny CRC-32 (IEEE) so the fixture writer is self-contained.
    fn crc32(data: &[u8]) -> u32 {
        let mut crc: u32 = 0xFFFF_FFFF;
        for &b in data {
            crc ^= b as u32;
            for _ in 0..8 {
                let mask = (crc & 1).wrapping_neg();
                crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
        !crc
    }

    /// Write a minimal Stored (uncompressed) zip containing `files` to `path`.
    /// No compression library: every entry is written verbatim, matching the
    /// only compression method `ZipProvider` supports.
    fn write_zip(path: &Path, files: &[(&str, &[u8])]) {
        struct CdRecord {
            name: String,
            crc: u32,
            size: u32,
            offset: u32,
        }

        let mut buf = Vec::new();
        let mut records = Vec::with_capacity(files.len());

        for &(name, content) in files {
            let crc = crc32(content);
            let name_bytes = name.as_bytes();
            let offset = buf.len() as u32;

            // Local file header.
            buf.extend_from_slice(&LFH_SIG.to_le_bytes());
            buf.extend_from_slice(&[0u8; 2]); // version needed
            buf.extend_from_slice(&[0u8; 2]); // flags
            buf.extend_from_slice(&0u16.to_le_bytes()); // method: Stored
            buf.extend_from_slice(&0u16.to_le_bytes()); // time
            buf.extend_from_slice(&0u16.to_le_bytes()); // date
            buf.extend_from_slice(&crc.to_le_bytes());
            buf.extend_from_slice(&(content.len() as u32).to_le_bytes()); // compressed size
            buf.extend_from_slice(&(content.len() as u32).to_le_bytes()); // uncompressed size
            buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(&0u16.to_le_bytes()); // extra len
            buf.extend_from_slice(name_bytes);
            buf.extend_from_slice(content);

            records.push(CdRecord { name: name.to_string(), crc, size: content.len() as u32, offset });
        }

        let cd_start = buf.len() as u32;
        for rec in &records {
            let name_bytes = rec.name.as_bytes();
            buf.extend_from_slice(&CDH_SIG.to_le_bytes());
            buf.extend_from_slice(&[0u8; 2]); // version made by
            buf.extend_from_slice(&[0u8; 2]); // version needed
            buf.extend_from_slice(&[0u8; 2]); // flags
            buf.extend_from_slice(&0u16.to_le_bytes()); // method
            buf.extend_from_slice(&0u16.to_le_bytes()); // time
            buf.extend_from_slice(&0u16.to_le_bytes()); // date
            buf.extend_from_slice(&rec.crc.to_le_bytes());
            buf.extend_from_slice(&rec.size.to_le_bytes()); // compressed size
            buf.extend_from_slice(&rec.size.to_le_bytes()); // uncompressed size
            buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            buf.extend_from_slice(&0u16.to_le_bytes()); // extra
            buf.extend_from_slice(&0u16.to_le_bytes()); // comment
            buf.extend_from_slice(&[0u8; 2]); // disk start
            buf.extend_from_slice(&[0u8; 2]); // internal attrs
            buf.extend_from_slice(&[0u8; 4]); // external attrs
            buf.extend_from_slice(&rec.offset.to_le_bytes());
            buf.extend_from_slice(name_bytes);
        }
        let cd_size = buf.len() as u32 - cd_start;

        buf.extend_from_slice(&EOCD_SIG.to_le_bytes());
        buf.extend_from_slice(&[0u8; 2]); // disk
        buf.extend_from_slice(&[0u8; 2]); // cd disk
        buf.extend_from_slice(&(records.len() as u16).to_le_bytes()); // entries on disk
        buf.extend_from_slice(&(records.len() as u16).to_le_bytes()); // total entries
        buf.extend_from_slice(&cd_size.to_le_bytes());
        buf.extend_from_slice(&cd_start.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // comment len

        std::fs::File::create(path).unwrap().write_all(&buf).unwrap();
    }

    /// Write the `vfs-provider` conformance reference tree as a Stored zip:
    /// `a.txt` = "hello", `sub/b.txt` = "world!".
    fn write_conformance_zip(path: &Path) {
        write_zip(path, vfs_provider::FIXTURE_FILES);
    }

    #[test]
    fn zip_provider_declares_immutable_read_access() {
        use vfs_provider::{Access, Provider};
        let dir = std::env::temp_dir().join(format!("vfs-zipcaps-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let zip = dir.join("t.zip");
        write_conformance_zip(&zip);

        let p = ZipProvider::open(&zip).expect("open zip");
        let caps = p.capabilities();
        assert_eq!(caps.access, Access::Read);
        assert!(caps.immutable, "a zip container never changes under us");
        assert!(!caps.slow);
        caps.validate().expect("declaration must be self-consistent");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zip_provider_passes_conformance() {
        let dir = std::env::temp_dir().join(format!("vfs-zipconf-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let zip = dir.join("t.zip");
        write_conformance_zip(&zip);

        let p: std::sync::Arc<dyn vfs_provider::Provider> =
            std::sync::Arc::new(ZipProvider::open(&zip).expect("open zip"));
        vfs_provider::assert_conformance(p);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
