//! SnapshotReader.

use crate::layout::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    TooSmall,
    BadMagic,
    BadVersion,
    RegionOutOfBounds,
    BadRoot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    NotADirectory,
    NotFound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Dir,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapStat {
    pub kind: NodeKind,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapDirEntry {
    pub name: String,
    pub kind: NodeKind,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapResolution {
    File {
        source: Vec<u8>,
        size: u64,
        mtime: i64,
        layer: u32,
        cache_key: [u8; 32],
    },
    Dir,
    NotFound,
}

pub struct SnapshotReader<'a> {
    bytes: &'a [u8],
    nodes_off: usize,
    node_count: u32,
    children_off: usize,
    child_count: u32,
    root_node: u32,
}

impl<'a> SnapshotReader<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self, LayoutError> {
        if bytes.len() < HEADER_SIZE {
            return Err(LayoutError::TooSmall);
        }
        if read_u32(bytes, H_MAGIC) != Some(MAGIC) {
            return Err(LayoutError::BadMagic);
        }
        if read_u32(bytes, H_VERSION) != Some(VERSION) {
            return Err(LayoutError::BadVersion);
        }
        let total_len = read_u32(bytes, H_TOTAL_LEN).unwrap() as usize;
        if total_len > bytes.len() {
            return Err(LayoutError::RegionOutOfBounds);
        }
        let node_count = read_u32(bytes, H_NODE_COUNT).unwrap();
        let nodes_off = read_u32(bytes, H_NODES_OFF).unwrap() as usize;
        let child_count = read_u32(bytes, H_CHILD_COUNT).unwrap();
        let children_off = read_u32(bytes, H_CHILDREN_OFF).unwrap() as usize;
        let strings_off = read_u32(bytes, H_STRINGS_OFF).unwrap() as usize;
        let strings_len = read_u32(bytes, H_STRINGS_LEN).unwrap() as usize;
        let root_node = read_u32(bytes, H_ROOT_NODE).unwrap();

        // Every region must fit within total_len.
        let nodes_end = nodes_off.checked_add((node_count as usize).checked_mul(NODE_SIZE).ok_or(LayoutError::RegionOutOfBounds)?).ok_or(LayoutError::RegionOutOfBounds)?;
        let children_end = children_off.checked_add((child_count as usize).checked_mul(CHILD_SIZE).ok_or(LayoutError::RegionOutOfBounds)?).ok_or(LayoutError::RegionOutOfBounds)?;
        let strings_end = strings_off.checked_add(strings_len).ok_or(LayoutError::RegionOutOfBounds)?;
        if nodes_end > total_len || children_end > total_len || strings_end > total_len {
            return Err(LayoutError::RegionOutOfBounds);
        }
        if node_count > 0 && root_node >= node_count {
            return Err(LayoutError::BadRoot);
        }
        Ok(SnapshotReader {
            bytes,
            nodes_off,
            node_count,
            children_off,
            child_count,
            root_node,
        })
    }

    pub fn generation(&self) -> u64 {
        read_u64(self.bytes, H_GENERATION).unwrap_or(0)
    }

    pub fn root(&self) -> u32 {
        self.root_node
    }

    fn node_base(&self, idx: u32) -> Option<usize> {
        if idx >= self.node_count {
            return None;
        }
        Some(self.nodes_off + idx as usize * NODE_SIZE)
    }

    fn node_kind(&self, idx: u32) -> Option<NodeKind> {
        let base = self.node_base(idx)?;
        match read_u8(self.bytes, base + N_KIND)? {
            KIND_DIR => Some(NodeKind::Dir),
            KIND_FILE => Some(NodeKind::File),
            _ => None,
        }
    }

    fn node_name(&self, idx: u32) -> Option<String> {
        let base = self.node_base(idx)?;
        let off = read_u32(self.bytes, base + N_NAME_OFF)? as usize;
        let len = read_u32(self.bytes, base + N_NAME_LEN)? as usize;
        let s = read_slice(self.bytes, off, len)?;
        Some(String::from_utf8_lossy(s).into_owned())
    }

    /// Resolve a folded path to a node index, bounds-checked throughout.
    fn lookup(&self, folded: &[&str]) -> Option<u32> {
        if self.node_count == 0 {
            return None;
        }
        let mut cur = self.root_node;
        for comp in folded {
            if self.node_kind(cur)? != NodeKind::Dir {
                return None;
            }
            cur = self.find_child(cur, comp.as_bytes())?;
        }
        Some(cur)
    }

    /// Binary-search a dir's child run for a folded name.
    fn find_child(&self, dir: u32, folded: &[u8]) -> Option<u32> {
        let base = self.node_base(dir)?;
        let first = read_u32(self.bytes, base + N_CHILD_FIRST)?;
        let count = read_u32(self.bytes, base + N_CHILD_COUNT)?;
        let (mut lo, mut hi) = (0u32, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let cidx = first.checked_add(mid)?;
            if cidx >= self.child_count {
                return None;
            }
            let cbase = self.children_off + cidx as usize * CHILD_SIZE;
            let foff = read_u32(self.bytes, cbase + C_FOLDED_OFF)? as usize;
            let flen = read_u32(self.bytes, cbase + C_FOLDED_LEN)? as usize;
            let name = read_slice(self.bytes, foff, flen)?;
            match name.cmp(folded) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    return read_u32(self.bytes, cbase + C_NODE);
                }
            }
        }
        None
    }

    pub fn getattr(&self, folded: &[&str]) -> Option<SnapStat> {
        let idx = self.lookup(folded)?;
        let base = self.node_base(idx)?;
        match self.node_kind(idx)? {
            NodeKind::Dir => Some(SnapStat { kind: NodeKind::Dir, size: 0, mtime: 0 }),
            NodeKind::File => Some(SnapStat {
                kind: NodeKind::File,
                size: read_u64(self.bytes, base + N_SIZE)?,
                mtime: read_i64(self.bytes, base + N_MTIME)?,
            }),
        }
    }

    fn file_resolution(&self, base: usize) -> Option<SnapResolution> {
        let off = read_u32(self.bytes, base + N_SOURCE_OFF)? as usize;
        let len = read_u32(self.bytes, base + N_SOURCE_LEN)? as usize;
        let source = read_slice(self.bytes, off, len)?.to_vec();
        Some(SnapResolution::File {
            source,
            size: read_u64(self.bytes, base + N_SIZE)?,
            mtime: read_i64(self.bytes, base + N_MTIME)?,
            layer: read_u32(self.bytes, base + N_LAYER)?,
            cache_key: read_key(self.bytes, base + N_CACHE_KEY)?,
        })
    }

    pub fn resolve(&self, folded: &[&str]) -> SnapResolution {
        let idx = match self.lookup(folded) {
            Some(i) => i,
            None => return SnapResolution::NotFound,
        };
        let base = match self.node_base(idx) {
            Some(b) => b,
            None => return SnapResolution::NotFound,
        };
        match self.node_kind(idx) {
            Some(NodeKind::Dir) => SnapResolution::Dir,
            Some(NodeKind::File) => self.file_resolution(base).unwrap_or(SnapResolution::NotFound),
            None => SnapResolution::NotFound,
        }
    }

    pub fn readdir(&self, folded: &[&str]) -> Result<Vec<SnapDirEntry>, ReadError> {
        let idx = self.lookup(folded).ok_or(ReadError::NotFound)?;
        if self.node_kind(idx).ok_or(ReadError::NotFound)? != NodeKind::Dir {
            return Err(ReadError::NotADirectory);
        }
        let base = self.node_base(idx).ok_or(ReadError::NotFound)?;
        let first = read_u32(self.bytes, base + N_CHILD_FIRST).ok_or(ReadError::NotFound)?;
        let count = read_u32(self.bytes, base + N_CHILD_COUNT).ok_or(ReadError::NotFound)?;
        let mut out = Vec::new();
        for k in 0..count {
            let cidx = match first.checked_add(k) {
                Some(c) if c < self.child_count => c,
                _ => break,
            };
            let cbase = self.children_off + cidx as usize * CHILD_SIZE;
            let node = match read_u32(self.bytes, cbase + C_NODE) {
                Some(n) => n,
                None => break,
            };
            let (name, kind, size, mtime) = match (self.node_name(node), self.node_kind(node)) {
                (Some(name), Some(kind)) => {
                    let nb = self.node_base(node).unwrap();
                    let (size, mtime) = match kind {
                        NodeKind::Dir => (0, 0),
                        NodeKind::File => (
                            read_u64(self.bytes, nb + N_SIZE).unwrap_or(0),
                            read_i64(self.bytes, nb + N_MTIME).unwrap_or(0),
                        ),
                    };
                    (name, kind, size, mtime)
                }
                _ => break,
            };
            out.push(SnapDirEntry { name, kind, size, mtime });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::SnapshotBuilder;

    // Fixture: /  ->  data(dir) -> {A.esp(file,10), b.esp(file,20), sub(dir)->{c.txt(file,30)}}
    fn fixture() -> Vec<u8> {
        let mut b = SnapshotBuilder::new();
        let c = b.add_file("c.txt", b"src/c", 30, 3, 1, [1; 32]);
        let sub = b.add_dir("sub", &[("c.txt".into(), c)]);
        let a = b.add_file("A.esp", b"src/a", 10, 1, 0, [0; 32]);
        let bb = b.add_file("b.esp", b"src/b", 20, 2, 2, [2; 32]);
        // folded names lowercased (caller's fold; here ASCII lowercase)
        let data = b.add_dir(
            "data",
            &[("a.esp".into(), a), ("b.esp".into(), bb), ("sub".into(), sub)],
        );
        let root = b.add_dir("", &[("data".into(), data)]);
        b.set_root(root);
        b.finish()
    }

    #[test]
    fn getattr_root_and_file() {
        let img = fixture();
        let r = SnapshotReader::open(&img).unwrap();
        assert_eq!(r.getattr(&[]).unwrap().kind, NodeKind::Dir);
        assert_eq!(
            r.getattr(&["data", "a.esp"]).unwrap(),
            SnapStat { kind: NodeKind::File, size: 10, mtime: 1 }
        );
        assert_eq!(r.getattr(&["data", "sub", "c.txt"]).unwrap().size, 30);
        assert_eq!(r.getattr(&["data", "missing"]), None);
    }

    #[test]
    fn resolve_file_carries_source_and_key() {
        let img = fixture();
        let r = SnapshotReader::open(&img).unwrap();
        match r.resolve(&["data", "b.esp"]) {
            SnapResolution::File { source, size, layer, cache_key, .. } => {
                assert_eq!(source, b"src/b");
                assert_eq!(size, 20);
                assert_eq!(layer, 2);
                assert_eq!(cache_key, [2; 32]);
            }
            other => panic!("expected file, got {other:?}"),
        }
        assert_eq!(r.resolve(&["data"]), SnapResolution::Dir);
        assert_eq!(r.resolve(&["nope"]), SnapResolution::NotFound);
    }

    #[test]
    fn readdir_is_case_insensitively_ordered() {
        let img = fixture();
        let r = SnapshotReader::open(&img).unwrap();
        let names: Vec<String> =
            r.readdir(&["data"]).unwrap().into_iter().map(|e| e.name).collect();
        // display names preserved; order follows folded sort: a.esp, b.esp, sub
        assert_eq!(names, vec!["A.esp", "b.esp", "sub"]);
    }

    #[test]
    fn readdir_on_file_errors() {
        let img = fixture();
        let r = SnapshotReader::open(&img).unwrap();
        assert_eq!(r.readdir(&["data", "a.esp"]), Err(ReadError::NotADirectory));
        assert_eq!(r.readdir(&["nope"]), Err(ReadError::NotFound));
    }

    #[test]
    fn open_rejects_bad_magic() {
        let mut img = fixture();
        img[0] ^= 0xFF;
        // `matches!` (not unwrap_err) so SnapshotReader needn't derive Debug.
        assert!(matches!(SnapshotReader::open(&img), Err(LayoutError::BadMagic)));
    }
}
