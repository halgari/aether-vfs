//! SnapshotBuilder.

use std::collections::HashMap;

use crate::layout::*;

struct NodeRec {
    kind: u8,
    layer: u32,
    name_off: u32,
    name_len: u32,
    child_first: u32,
    child_count: u32,
    source_off: u32,
    source_len: u32,
    size: u64,
    mtime: i64,
    cache_key: [u8; 32],
}

struct ChildRec {
    folded_off: u32,
    folded_len: u32,
    node: u32,
}

/// Builds a snapshot image bottom-up (children before their parent dir).
pub struct SnapshotBuilder {
    strings: Vec<u8>,
    intern: HashMap<Vec<u8>, u32>,
    nodes: Vec<NodeRec>,
    children: Vec<ChildRec>,
    root: u32,
}

impl SnapshotBuilder {
    pub fn new() -> Self {
        SnapshotBuilder {
            strings: Vec::new(),
            intern: HashMap::new(),
            nodes: Vec::new(),
            children: Vec::new(),
            root: 0,
        }
    }

    fn intern(&mut self, bytes: &[u8]) -> (u32, u32) {
        if let Some(&off) = self.intern.get(bytes) {
            return (off, bytes.len() as u32);
        }
        let off = self.strings.len() as u32;
        self.strings.extend_from_slice(bytes);
        self.intern.insert(bytes.to_vec(), off);
        (off, bytes.len() as u32)
    }

    pub fn add_file(
        &mut self,
        display: &str,
        source: &[u8],
        size: u64,
        mtime: i64,
        layer: u32,
        cache_key: [u8; 32],
    ) -> u32 {
        let (name_off, name_len) = self.intern(display.as_bytes());
        let (source_off, source_len) = self.intern(source);
        let id = self.nodes.len() as u32;
        self.nodes.push(NodeRec {
            kind: KIND_FILE,
            layer,
            name_off,
            name_len,
            child_first: 0,
            child_count: 0,
            source_off,
            source_len,
            size,
            mtime,
            cache_key,
        });
        id
    }

    pub fn add_tombstone(&mut self, display: &str) -> u32 {
        let (name_off, name_len) = self.intern(display.as_bytes());
        let id = self.nodes.len() as u32;
        self.nodes.push(NodeRec {
            kind: KIND_TOMBSTONE,
            layer: 0,
            name_off,
            name_len,
            child_first: 0,
            child_count: 0,
            source_off: 0,
            source_len: 0,
            size: 0,
            mtime: 0,
            cache_key: [0; 32],
        });
        id
    }

    pub fn add_dir(&mut self, display: &str, children: &[(String, u32)]) -> u32 {
        let mut sorted = children.to_vec();
        sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let child_first = self.children.len() as u32;
        for (folded, node) in &sorted {
            let (folded_off, folded_len) = self.intern(folded.as_bytes());
            self.children.push(ChildRec { folded_off, folded_len, node: *node });
        }
        let (name_off, name_len) = self.intern(display.as_bytes());
        let id = self.nodes.len() as u32;
        self.nodes.push(NodeRec {
            kind: KIND_DIR,
            layer: 0,
            name_off,
            name_len,
            child_first,
            child_count: sorted.len() as u32,
            source_off: 0,
            source_len: 0,
            size: 0,
            mtime: 0,
            cache_key: [0; 32],
        });
        id
    }

    pub fn set_root(&mut self, node: u32) {
        self.root = node;
    }

    pub fn finish(self) -> Vec<u8> {
        let node_count = self.nodes.len();
        let child_count = self.children.len();
        let nodes_off = HEADER_SIZE;
        let children_off = nodes_off + node_count * NODE_SIZE;
        let strings_off = children_off + child_count * CHILD_SIZE;
        let total_len = strings_off + self.strings.len();

        let mut b = vec![0u8; total_len];
        write_u32(&mut b, H_MAGIC, MAGIC);
        write_u32(&mut b, H_VERSION, VERSION);
        write_u64(&mut b, H_GENERATION, 0);
        write_u32(&mut b, H_TOTAL_LEN, total_len as u32);
        write_u32(&mut b, H_ROOT_NODE, self.root);
        write_u32(&mut b, H_NODE_COUNT, node_count as u32);
        write_u32(&mut b, H_NODES_OFF, nodes_off as u32);
        write_u32(&mut b, H_CHILD_COUNT, child_count as u32);
        write_u32(&mut b, H_CHILDREN_OFF, children_off as u32);
        write_u32(&mut b, H_STRINGS_LEN, self.strings.len() as u32);
        write_u32(&mut b, H_STRINGS_OFF, strings_off as u32);

        let s = strings_off as u32;
        for (i, n) in self.nodes.iter().enumerate() {
            let base = nodes_off + i * NODE_SIZE;
            write_u8(&mut b, base + N_KIND, n.kind);
            write_u32(&mut b, base + N_LAYER, n.layer);
            write_u32(&mut b, base + N_NAME_OFF, s + n.name_off);
            write_u32(&mut b, base + N_NAME_LEN, n.name_len);
            write_u32(&mut b, base + N_CHILD_FIRST, n.child_first);
            write_u32(&mut b, base + N_CHILD_COUNT, n.child_count);
            write_u32(&mut b, base + N_SOURCE_OFF, s + n.source_off);
            write_u32(&mut b, base + N_SOURCE_LEN, n.source_len);
            write_u64(&mut b, base + N_SIZE, n.size);
            write_i64(&mut b, base + N_MTIME, n.mtime);
            write_key(&mut b, base + N_CACHE_KEY, &n.cache_key);
        }
        for (j, c) in self.children.iter().enumerate() {
            let base = children_off + j * CHILD_SIZE;
            write_u32(&mut b, base + C_FOLDED_OFF, s + c.folded_off);
            write_u32(&mut b, base + C_FOLDED_LEN, c.folded_len);
            write_u32(&mut b, base + C_NODE, c.node);
        }
        write_bytes(&mut b, strings_off, &self.strings);
        b
    }
}

impl Default for SnapshotBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_reflects_counts_and_offsets() {
        let mut bld = SnapshotBuilder::new();
        let f1 = bld.add_file("a.esp", b"src/a", 10, 1, 0, [0; 32]);
        let f2 = bld.add_file("b.esp", b"src/b", 20, 2, 0, [0; 32]);
        let root = bld.add_dir("", &[("a.esp".into(), f1), ("b.esp".into(), f2)]);
        bld.set_root(root);
        let img = bld.finish();

        assert_eq!(read_u32(&img, H_MAGIC), Some(MAGIC));
        assert_eq!(read_u32(&img, H_VERSION), Some(VERSION));
        assert_eq!(read_u32(&img, H_NODE_COUNT), Some(3));
        assert_eq!(read_u32(&img, H_CHILD_COUNT), Some(2));
        assert_eq!(read_u32(&img, H_ROOT_NODE), Some(root));
        assert_eq!(read_u32(&img, H_TOTAL_LEN), Some(img.len() as u32));
        // nodes start right after the header
        assert_eq!(read_u32(&img, H_NODES_OFF), Some(HEADER_SIZE as u32));
    }

    #[test]
    fn tombstone_node_kind_is_written() {
        let mut bld = SnapshotBuilder::new();
        let t = bld.add_tombstone("gone.esp");
        bld.set_root(t);
        let img = bld.finish();
        assert_eq!(read_u8(&img, HEADER_SIZE + N_KIND), Some(KIND_TOMBSTONE));
    }

    #[test]
    fn file_node_fields_are_written() {
        let mut bld = SnapshotBuilder::new();
        let f = bld.add_file("a.esp", b"src/a", 10, 7, 3, [9; 32]);
        bld.set_root(f);
        let img = bld.finish();
        let base = HEADER_SIZE; // node 0
        assert_eq!(read_u8(&img, base + N_KIND), Some(KIND_FILE));
        assert_eq!(read_u64(&img, base + N_SIZE), Some(10));
        assert_eq!(read_i64(&img, base + N_MTIME), Some(7));
        assert_eq!(read_u32(&img, base + N_LAYER), Some(3));
        assert_eq!(read_key(&img, base + N_CACHE_KEY), Some([9; 32]));
    }
}
