//! Merged tree build + queries.

use std::collections::BTreeMap;

use crate::cachekey::compute_cache_key;
use crate::casefold::fold;
use crate::model::{BuildError, EntryKind, InputEntry, Layer, LayerId, Resolution, SourceId};
use crate::path::normalize_vpath;

#[derive(Debug)]
struct Node {
    name: String,
    entry: NodeEntry,
}

#[derive(Debug)]
enum NodeEntry {
    File(FileNode),
    Dir(DirNode),
}

#[derive(Debug)]
struct FileNode {
    source: SourceId,
    size: u64,
    mtime: i64,
    layer: LayerId,
}

#[derive(Debug)]
struct DirNode {
    children: BTreeMap<String, u32>, // key = folded name → node index
}

#[derive(Debug)]
pub struct VfsTree {
    nodes: Vec<Node>, // nodes[0] is the root dir
}

pub fn build(layers: Vec<Layer>) -> Result<VfsTree, BuildError> {
    let mut tree = VfsTree {
        nodes: vec![Node {
            name: String::new(),
            entry: NodeEntry::Dir(DirNode { children: BTreeMap::new() }),
        }],
    };
    for layer in &layers {
        for entry in &layer.entries {
            let norm = normalize_vpath(&entry.vpath).map_err(|_| BuildError::EscapesRoot)?;
            if norm.is_empty() {
                return Err(BuildError::EmptyPath);
            }
            let comps: Vec<&str> = norm.split('/').collect();
            match entry.kind {
                EntryKind::Tombstone => tree.remove_path(&comps),
                EntryKind::Dir => {
                    tree.ensure_dir_path(&comps);
                }
                EntryKind::File => tree.insert_file(&comps, entry, layer.id),
            }
        }
    }
    Ok(tree)
}

impl VfsTree {
    pub fn resolve(&self, vpath: &str) -> Resolution {
        let norm = match normalize_vpath(vpath) {
            Ok(n) => n,
            Err(_) => return Resolution::NotFound,
        };
        match self.find(&norm) {
            Some(id) => match &self.nodes[id as usize].entry {
                NodeEntry::Dir(_) => Resolution::Dir,
                NodeEntry::File(f) => Resolution::File {
                    source: f.source.clone(),
                    size: f.size,
                    mtime: f.mtime,
                    layer: f.layer,
                    cache_key: compute_cache_key(&f.source, f.size, f.mtime),
                },
            },
            None => Resolution::NotFound,
        }
    }

    pub fn getattr(&self, vpath: &str) -> Option<crate::model::Stat> {
        use crate::model::{NodeKind, Stat};
        let norm = normalize_vpath(vpath).ok()?;
        let id = self.find(&norm)?;
        Some(match &self.nodes[id as usize].entry {
            NodeEntry::Dir(_) => Stat { kind: NodeKind::Dir, size: 0, mtime: 0 },
            NodeEntry::File(f) => Stat { kind: NodeKind::File, size: f.size, mtime: f.mtime },
        })
    }

    pub fn readdir(
        &self,
        vpath: &str,
        filter: Option<&str>,
    ) -> Result<Vec<crate::model::DirEntry>, crate::model::VfsError> {
        use crate::casefold::cmp_ci;
        use crate::model::{DirEntry, NodeKind, VfsError};
        use crate::wildcard::wildcard_match;

        let norm = normalize_vpath(vpath).map_err(|_| VfsError::NotFound)?;
        let id = self.find(&norm).ok_or(VfsError::NotFound)?;
        let dir = match &self.nodes[id as usize].entry {
            NodeEntry::Dir(d) => d,
            NodeEntry::File(_) => return Err(VfsError::NotADirectory),
        };

        let mut out: Vec<DirEntry> = dir
            .children
            .values()
            .map(|&cid| {
                let node = &self.nodes[cid as usize];
                match &node.entry {
                    NodeEntry::Dir(_) => DirEntry {
                        name: node.name.clone(),
                        kind: NodeKind::Dir,
                        size: 0,
                        mtime: 0,
                    },
                    NodeEntry::File(f) => DirEntry {
                        name: node.name.clone(),
                        kind: NodeKind::File,
                        size: f.size,
                        mtime: f.mtime,
                    },
                }
            })
            .filter(|e| match filter {
                Some(pat) => wildcard_match(pat, &e.name),
                None => true,
            })
            .collect();

        out.sort_by(|a, b| cmp_ci(&a.name, &b.name));
        Ok(out)
    }

    fn find(&self, norm: &str) -> Option<u32> {
        let mut cur = 0u32;
        if norm.is_empty() {
            return Some(0);
        }
        for comp in norm.split('/') {
            let key = fold(comp);
            match &self.nodes[cur as usize].entry {
                NodeEntry::Dir(d) => cur = *d.children.get(&key)?,
                NodeEntry::File(_) => return None,
            }
        }
        Some(cur)
    }

    /// Push a fresh node, return its index.
    fn push(&mut self, name: &str, entry: NodeEntry) -> u32 {
        let id = self.nodes.len() as u32;
        self.nodes.push(Node { name: name.to_string(), entry });
        id
    }

    /// Look up a direct child index by folded component name.
    fn child(&self, parent: u32, key: &str) -> Option<u32> {
        match &self.nodes[parent as usize].entry {
            NodeEntry::Dir(d) => d.children.get(key).copied(),
            NodeEntry::File(_) => None,
        }
    }

    fn set_child(&mut self, parent: u32, key: String, id: u32) {
        if let NodeEntry::Dir(d) = &mut self.nodes[parent as usize].entry {
            d.children.insert(key, id);
        }
    }

    /// Ensure every component of `comps` exists as a directory; return the leaf's id.
    /// If an existing node on the path is a File, it is replaced by a Dir (higher wins).
    fn ensure_dir_path(&mut self, comps: &[&str]) -> u32 {
        let mut cur = 0u32;
        for comp in comps {
            let key = fold(comp);
            match self.child(cur, &key) {
                Some(id) => {
                    if matches!(self.nodes[id as usize].entry, NodeEntry::File(_)) {
                        // Replace file with an empty dir; name takes this layer's casing.
                        self.nodes[id as usize].name = comp.to_string();
                        self.nodes[id as usize].entry =
                            NodeEntry::Dir(DirNode { children: BTreeMap::new() });
                    }
                    cur = id;
                }
                None => {
                    let id = self.push(comp, NodeEntry::Dir(DirNode { children: BTreeMap::new() }));
                    self.set_child(cur, key, id);
                    cur = id;
                }
            }
        }
        cur
    }

    /// Insert a file at `comps`, creating parent dirs; replaces any existing node.
    fn insert_file(&mut self, comps: &[&str], entry: &InputEntry, layer: LayerId) {
        let (leaf, parents) = comps.split_last().expect("build guarantees non-empty");
        let parent = self.ensure_dir_path(parents);
        let key = fold(leaf);
        let file = NodeEntry::File(FileNode {
            source: entry.source.clone(),
            size: entry.size,
            mtime: entry.mtime,
            layer,
        });
        match self.child(parent, &key) {
            Some(id) => {
                self.nodes[id as usize].name = leaf.to_string();
                self.nodes[id as usize].entry = file;
            }
            None => {
                let id = self.push(leaf, file);
                self.set_child(parent, key, id);
            }
        }
    }

    /// Remove the node at `comps` from its parent (tombstone / whiteout). Orphaned
    /// subtree nodes remain in the arena but become unreachable — acceptable for MVP.
    fn remove_path(&mut self, comps: &[&str]) {
        let (leaf, parents) = match comps.split_last() {
            Some(x) => x,
            None => return,
        };
        // Walk parents without creating anything.
        let mut cur = 0u32;
        for comp in parents {
            match self.child(cur, &fold(comp)) {
                Some(id) if matches!(self.nodes[id as usize].entry, NodeEntry::Dir(_)) => cur = id,
                _ => return, // path doesn't exist as a dir; nothing to remove
            }
        }
        if let NodeEntry::Dir(d) = &mut self.nodes[cur as usize].entry {
            d.children.remove(&fold(leaf));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(vpath: &str, source: &str, size: u64, mtime: i64) -> InputEntry {
        InputEntry { vpath: vpath.into(), kind: EntryKind::File, source: source.into(), size, mtime }
    }
    fn dir(vpath: &str) -> InputEntry {
        InputEntry { vpath: vpath.into(), kind: EntryKind::Dir, source: "".into(), size: 0, mtime: 0 }
    }
    fn tomb(vpath: &str) -> InputEntry {
        InputEntry { vpath: vpath.into(), kind: EntryKind::Tombstone, source: "".into(), size: 0, mtime: 0 }
    }
    fn layer(id: u32, entries: Vec<InputEntry>) -> Layer {
        Layer { id: LayerId(id), entries }
    }

    #[test]
    fn higher_layer_wins() {
        let t = build(vec![
            layer(0, vec![file("data/a.esp", "L0/a", 1, 1)]),
            layer(1, vec![file("data/a.esp", "L1/a", 2, 2)]),
        ])
        .unwrap();
        match t.resolve("data/a.esp") {
            Resolution::File { source, size, layer, .. } => {
                assert_eq!(source, SourceId::from("L1/a"));
                assert_eq!(size, 2);
                assert_eq!(layer, LayerId(1));
            }
            other => panic!("expected file, got {other:?}"),
        }
    }

    #[test]
    fn directories_union() {
        let t = build(vec![
            layer(0, vec![file("data/a.esp", "L0/a", 1, 1)]),
            layer(1, vec![file("data/b.esp", "L1/b", 1, 1)]),
        ])
        .unwrap();
        assert!(matches!(t.resolve("data/a.esp"), Resolution::File { .. }));
        assert!(matches!(t.resolve("data/b.esp"), Resolution::File { .. }));
        assert!(matches!(t.resolve("data"), Resolution::Dir));
    }

    #[test]
    fn resolve_missing_is_notfound() {
        let t = build(vec![layer(0, vec![file("data/a.esp", "L0/a", 1, 1)])]).unwrap();
        assert_eq!(t.resolve("data/missing"), Resolution::NotFound);
    }

    #[test]
    fn tombstone_hides_lower_layer() {
        let t = build(vec![
            layer(0, vec![file("data/a.esp", "L0/a", 1, 1)]),
            layer(1, vec![tomb("data/a.esp")]),
        ])
        .unwrap();
        assert_eq!(t.resolve("data/a.esp"), Resolution::NotFound);
    }

    #[test]
    fn higher_layer_resurrects_tombstone() {
        let t = build(vec![
            layer(0, vec![file("data/a.esp", "L0/a", 1, 1)]),
            layer(1, vec![tomb("data/a.esp")]),
            layer(2, vec![file("data/a.esp", "L2/a", 3, 3)]),
        ])
        .unwrap();
        match t.resolve("data/a.esp") {
            Resolution::File { source, .. } => assert_eq!(source, SourceId::from("L2/a")),
            other => panic!("expected resurrected file, got {other:?}"),
        }
    }

    #[test]
    fn directory_tombstone_hides_subtree() {
        let t = build(vec![
            layer(0, vec![file("data/sub/a.esp", "L0/a", 1, 1)]),
            layer(1, vec![tomb("data/sub")]),
        ])
        .unwrap();
        assert_eq!(t.resolve("data/sub"), Resolution::NotFound);
        assert_eq!(t.resolve("data/sub/a.esp"), Resolution::NotFound);
    }

    #[test]
    fn file_dir_conflict_higher_wins() {
        // Lower layer: "x" is a file. Higher layer: "x" is a dir with a child.
        let t = build(vec![
            layer(0, vec![file("x", "L0/x", 1, 1)]),
            layer(1, vec![file("x/child", "L1/child", 1, 1)]),
        ])
        .unwrap();
        assert!(matches!(t.resolve("x"), Resolution::Dir));
        assert!(matches!(t.resolve("x/child"), Resolution::File { .. }));
    }

    #[test]
    fn empty_input_path_errors() {
        let err = build(vec![layer(0, vec![file("", "s", 1, 1)])]).unwrap_err();
        assert_eq!(err, BuildError::EmptyPath);
    }

    #[test]
    fn cache_key_present_on_resolved_file() {
        let t = build(vec![layer(0, vec![file("a", "s", 7, 8)])]).unwrap();
        match t.resolve("a") {
            Resolution::File { cache_key, source, size, mtime, .. } => {
                assert_eq!(cache_key, compute_cache_key(&source, size, mtime));
            }
            other => panic!("expected file, got {other:?}"),
        }
    }

    #[test]
    fn getattr_file_reports_size_and_mtime() {
        use crate::model::{NodeKind, Stat};
        let t = build(vec![layer(0, vec![file("data/a.esp", "s", 123, 456)])]).unwrap();
        assert_eq!(
            t.getattr("data/a.esp"),
            Some(Stat { kind: NodeKind::File, size: 123, mtime: 456 })
        );
    }

    #[test]
    fn getattr_dir_reports_dir_kind() {
        use crate::model::{NodeKind, Stat};
        let t = build(vec![layer(0, vec![file("data/a.esp", "s", 1, 1)])]).unwrap();
        assert_eq!(
            t.getattr("data"),
            Some(Stat { kind: NodeKind::Dir, size: 0, mtime: 0 })
        );
    }

    #[test]
    fn getattr_missing_is_none() {
        let t = build(vec![layer(0, vec![file("data/a.esp", "s", 1, 1)])]).unwrap();
        assert_eq!(t.getattr("nope"), None);
    }

    #[test]
    fn readdir_merges_and_sorts_case_insensitively() {
        let t = build(vec![
            layer(0, vec![file("d/Zebra.esp", "s", 1, 1), file("d/apple.esp", "s", 1, 1)]),
            layer(1, vec![file("d/Mango.esp", "s", 1, 1)]),
        ])
        .unwrap();
        let names: Vec<String> = t.readdir("d", None).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["apple.esp", "Mango.esp", "Zebra.esp"]);
    }

    #[test]
    fn readdir_honors_tombstones() {
        let t = build(vec![
            layer(0, vec![file("d/a.esp", "s", 1, 1), file("d/b.esp", "s", 1, 1)]),
            layer(1, vec![tomb("d/a.esp")]),
        ])
        .unwrap();
        let names: Vec<String> = t.readdir("d", None).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["b.esp"]);
    }

    #[test]
    fn readdir_applies_wildcard_filter() {
        let t = build(vec![layer(
            0,
            vec![file("d/a.esp", "s", 1, 1), file("d/b.txt", "s", 1, 1), file("d/c.esp", "s", 1, 1)],
        )])
        .unwrap();
        let names: Vec<String> =
            t.readdir("d", Some("*.esp")).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a.esp", "c.esp"]);
    }

    #[test]
    fn readdir_on_file_is_not_a_directory() {
        use crate::model::VfsError;
        let t = build(vec![layer(0, vec![file("d/a.esp", "s", 1, 1)])]).unwrap();
        assert_eq!(t.readdir("d/a.esp", None).unwrap_err(), VfsError::NotADirectory);
    }

    #[test]
    fn readdir_missing_is_not_found() {
        use crate::model::VfsError;
        let t = build(vec![layer(0, vec![file("d/a.esp", "s", 1, 1)])]).unwrap();
        assert_eq!(t.readdir("nope", None).unwrap_err(), VfsError::NotFound);
    }
}
