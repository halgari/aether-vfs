//! Stateful director open-file table: OPEN/READ/CLOSE over zip windows and disk.
//!
//! **A1:** OPEN keeps an OS `File` open.
//! **A2:** READ clones `Arc` and drops map mutex before I/O.
//! **B5:** Sequential readahead into a small per-fh buffer.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use vfs_core::{decode, NodeKind, Resolution, Source, VfsTree};
use vfs_protocol::{
    OpenResp, OPEN_WRITE, ST_BAD_FH, ST_BAD_REQUEST, ST_IO_ERROR, ST_IS_DIR, ST_NOT_FOUND, ST_OK,
};

/// Prefetch size for sequential access (**B5**).
const READAHEAD_SIZE: usize = 256 * 1024;

struct Readahead {
    offset: u64,
    data: Vec<u8>,
}

/// Live open file: seek base + kept OS handle + optional readahead.
struct LiveFile {
    size: u64,
    base_offset: u64,
    file: Mutex<File>,
    /// Last successful read end offset (for sequential detection).
    last_end: Mutex<u64>,
    readahead: Mutex<Option<Readahead>>,
}

enum OpenKind {
    File(Arc<LiveFile>),
    Dir { size: u64 },
}

struct OpenEntry {
    kind: OpenKind,
}

pub struct OpenTable {
    next: AtomicU64,
    map: Mutex<HashMap<u64, OpenEntry>>,
}

impl Default for OpenTable {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenTable {
    pub fn new() -> Self {
        OpenTable {
            next: AtomicU64::new(1),
            map: Mutex::new(HashMap::new()),
        }
    }

    pub fn open(&self, tree: &VfsTree, path: &str, flags: u32) -> Result<OpenResp, i32> {
        if flags & OPEN_WRITE != 0 {
            return Err(ST_BAD_REQUEST);
        }
        let vpath = path.trim_start_matches(['/', '\\']);
        let res = tree.resolve(vpath);
        match res {
            Resolution::NotFound | Resolution::Tombstone => Err(ST_NOT_FOUND),
            Resolution::Dir => {
                let fh = self.alloc_fh();
                let mut g = self.map.lock().map_err(|_| ST_IO_ERROR)?;
                g.insert(
                    fh,
                    OpenEntry {
                        kind: OpenKind::Dir { size: 0 },
                    },
                );
                Ok(OpenResp {
                    fh,
                    size: 0,
                    is_dir: true,
                })
            }
            Resolution::File {
                source,
                size,
                mtime: _,
                layer: _,
                cache_key: _,
            } => {
                let live = match decode(&source.0) {
                    Source::ZipWindow { offset, container } => {
                        let c = String::from_utf8_lossy(container).into_owned();
                        let path = PathBuf::from(c);
                        let f = File::open(&path).map_err(|_| ST_IO_ERROR)?;
                        Arc::new(LiveFile {
                            size,
                            base_offset: offset,
                            file: Mutex::new(f),
                            last_end: Mutex::new(0),
                            readahead: Mutex::new(None),
                        })
                    }
                    Source::Disk(bytes) => {
                        let p = String::from_utf8_lossy(bytes).into_owned();
                        let path = PathBuf::from(p);
                        let f = File::open(&path).map_err(|_| ST_IO_ERROR)?;
                        Arc::new(LiveFile {
                            size,
                            base_offset: 0,
                            file: Mutex::new(f),
                            last_end: Mutex::new(0),
                            readahead: Mutex::new(None),
                        })
                    }
                };
                let fh = self.alloc_fh();
                let mut g = self.map.lock().map_err(|_| ST_IO_ERROR)?;
                g.insert(
                    fh,
                    OpenEntry {
                        kind: OpenKind::File(live),
                    },
                );
                Ok(OpenResp {
                    fh,
                    size,
                    is_dir: false,
                })
            }
        }
    }

    pub fn open_with_getattr(&self, tree: &VfsTree, path: &str, flags: u32) -> Result<OpenResp, i32> {
        match self.open(tree, path, flags) {
            Err(ST_NOT_FOUND) => {
                if let Some(s) = tree.getattr(path.trim_start_matches(['/', '\\'])) {
                    if s.kind == NodeKind::Dir {
                        let fh = self.alloc_fh();
                        let mut g = self.map.lock().map_err(|_| ST_IO_ERROR)?;
                        g.insert(
                            fh,
                            OpenEntry {
                                kind: OpenKind::Dir { size: 0 },
                            },
                        );
                        return Ok(OpenResp {
                            fh,
                            size: 0,
                            is_dir: true,
                        });
                    }
                }
                Err(ST_NOT_FOUND)
            }
            other => other,
        }
    }

    pub fn read(&self, fh: u64, offset: u64, len: u32, max_data: usize) -> Result<Vec<u8>, i32> {
        let live = {
            let g = self.map.lock().map_err(|_| ST_IO_ERROR)?;
            let ent = g.get(&fh).ok_or(ST_BAD_FH)?;
            match &ent.kind {
                OpenKind::Dir { .. } => return Err(ST_IS_DIR),
                OpenKind::File(live) => Arc::clone(live),
            }
        };
        if offset >= live.size {
            return Ok(Vec::new());
        }
        let remain = (live.size - offset) as usize;
        let want = (len as usize).min(max_data).min(remain);
        if want == 0 {
            return Ok(Vec::new());
        }

        // **B5:** serve from readahead if it covers this range.
        if let Ok(ra) = live.readahead.lock() {
            if let Some(ref r) = *ra {
                if offset >= r.offset {
                    let skip = (offset - r.offset) as usize;
                    if skip < r.data.len() {
                        let n = want.min(r.data.len() - skip);
                        let mut buf = r.data[skip..skip + n].to_vec();
                        // Partial hit: still return what we have; caller may re-request rest.
                        if n == want {
                            if let Ok(mut le) = live.last_end.lock() {
                                *le = offset + n as u64;
                            }
                            return Ok(buf);
                        }
                        // Fall through to full read if readahead is only partial
                        let _ = buf;
                    }
                }
            }
        }

        let mut buf = vec![0u8; want];
        let abs = live.base_offset.saturating_add(offset);
        {
            let mut f = live.file.lock().map_err(|_| ST_IO_ERROR)?;
            f.seek(SeekFrom::Start(abs)).map_err(|_| ST_IO_ERROR)?;
            let n = f.read(&mut buf).map_err(|_| ST_IO_ERROR)?;
            buf.truncate(n);
        }
        let n = buf.len();
        let end = offset + n as u64;

        // **B5:** if sequential, prefetch next window under the file lock.
        let sequential = live
            .last_end
            .lock()
            .map(|le| *le == offset || *le == 0)
            .unwrap_or(false);
        if sequential && end < live.size {
            let pref = READAHEAD_SIZE.min((live.size - end) as usize);
            if pref > 0 {
                let mut pre = vec![0u8; pref];
                if let Ok(mut f) = live.file.lock() {
                    let abs2 = live.base_offset.saturating_add(end);
                    if f.seek(SeekFrom::Start(abs2)).is_ok() {
                        if let Ok(pn) = f.read(&mut pre) {
                            pre.truncate(pn);
                            if let Ok(mut ra) = live.readahead.lock() {
                                *ra = Some(Readahead {
                                    offset: end,
                                    data: pre,
                                });
                            }
                        }
                    }
                }
            }
        }
        if let Ok(mut le) = live.last_end.lock() {
            *le = end;
        }
        Ok(buf)
    }

    /// Read into a caller buffer (for bulk arena path — one less alloc when possible).
    pub fn read_into(
        &self,
        fh: u64,
        offset: u64,
        max_data: usize,
        out: &mut [u8],
    ) -> Result<usize, i32> {
        let data = self.read(fh, offset, max_data as u32, max_data.min(out.len()))?;
        let n = data.len().min(out.len());
        out[..n].copy_from_slice(&data[..n]);
        Ok(n)
    }

    pub fn close(&self, fh: u64) -> Result<(), i32> {
        let mut g = self.map.lock().map_err(|_| ST_IO_ERROR)?;
        if g.remove(&fh).is_some() {
            Ok(())
        } else {
            Err(ST_BAD_FH)
        }
    }

    fn alloc_fh(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

pub fn max_read_data(payload_cap: u32) -> usize {
    payload_cap.saturating_sub(8) as usize
}

pub fn status_ok() -> i32 {
    ST_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use vfs_core::{build, encode_zip_window, EntryKind, InputEntry, Layer, LayerId};

    fn disk_tree(path: &std::path::Path, size: u64) -> VfsTree {
        let src = path.to_string_lossy().into_owned();
        build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/f.bin".into(),
                kind: EntryKind::File,
                source: src.as_str().into(),
                size,
                mtime: 1,
            }],
        }])
        .unwrap()
    }

    #[test]
    fn open_read_close_disk_source() {
        let dir = std::env::temp_dir().join(format!("vfs-ot-disk-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("f.bin");
        let content = b"fuse-disk-bytes";
        {
            let mut f = File::create(&file).unwrap();
            f.write_all(content).unwrap();
        }
        let tree = disk_tree(&file, content.len() as u64);
        let table = OpenTable::new();
        let open = table.open(&tree, "data/f.bin", 0).unwrap();
        assert!(!open.is_dir);
        assert_eq!(open.size, content.len() as u64);
        let got = table
            .read(open.fh, 0, content.len() as u32, 4096)
            .unwrap();
        assert_eq!(got, content);
        table.close(open.fh).unwrap();
        assert_eq!(table.read(open.fh, 0, 4, 4096).unwrap_err(), ST_BAD_FH);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_fragments_honor_max_data() {
        let dir = std::env::temp_dir().join(format!("vfs-ot-frag-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("big.bin");
        let content: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &content).unwrap();
        let tree = disk_tree(&file, content.len() as u64);
        let table = OpenTable::new();
        let open = table.open(&tree, "data/f.bin", 0).unwrap();
        let chunk = table.read(open.fh, 0, 1000, 100).unwrap();
        assert_eq!(chunk.len(), 100);
        assert_eq!(&chunk[..], &content[..100]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_tiny_stored_zip(path: &std::path::Path, name: &str, data: &[u8]) {
        use std::io::Write;
        let mut f = File::create(path).unwrap();
        let name_b = name.as_bytes();
        let mut local = Vec::new();
        local.extend_from_slice(&0x04034b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u32.to_le_bytes());
        local.extend_from_slice(&(data.len() as u32).to_le_bytes());
        local.extend_from_slice(&(data.len() as u32).to_le_bytes());
        local.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name_b);
        local.extend_from_slice(data);
        f.write_all(&local).unwrap();
        let cd_off = local.len() as u32;
        let mut cd = Vec::new();
        cd.extend_from_slice(&0x02014b50u32.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&20u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
        cd.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u16.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(&0u32.to_le_bytes());
        cd.extend_from_slice(name_b);
        f.write_all(&cd).unwrap();
        let mut eocd = Vec::new();
        eocd.extend_from_slice(&0x06054b50u32.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes());
        eocd.extend_from_slice(&1u16.to_le_bytes());
        eocd.extend_from_slice(&1u16.to_le_bytes());
        eocd.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        eocd.extend_from_slice(&cd_off.to_le_bytes());
        eocd.extend_from_slice(&0u16.to_le_bytes());
        f.write_all(&eocd).unwrap();
    }

    #[test]
    fn open_read_zip_window() {
        let dir = std::env::temp_dir().join(format!("vfs-ot-zip-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let zip_path = dir.join("t.zip");
        let data = b"hello-world";
        write_tiny_stored_zip(&zip_path, "hello.txt", data);
        let data_off = 30u64 + 9;
        let src = encode_zip_window(data_off, &zip_path.to_string_lossy());
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "hello.txt".into(),
                kind: EntryKind::File,
                source: vfs_core::SourceId::new(src),
                size: data.len() as u64,
                mtime: 1,
            }],
        }])
        .unwrap();
        let table = OpenTable::new();
        let open = table.open(&tree, "hello.txt", 0).unwrap();
        let a = table.read(open.fh, 0, 5, 4096).unwrap();
        assert_eq!(a, b"hello");
        let b = table.read(open.fh, 6, 5, 4096).unwrap();
        assert_eq!(b, b"world");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
