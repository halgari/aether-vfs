//! Feature-gated bridge from a vfs-core VfsTree to a snapshot image.

use crate::builder::SnapshotBuilder;
use vfs_core::{WalkNode, WalkNodeKind};

/// Flatten a merged vfs-core tree into a snapshot image. Post-order walk means
/// each node's children are already built when the parent dir is added.
pub fn flatten(tree: &vfs_core::VfsTree) -> Vec<u8> {
    let mut builder = SnapshotBuilder::new();
    // Map vfs-core node id → snapshot node index as we build bottom-up.
    let mut id_map: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut root_snap: u32 = 0;

    tree.walk_postorder(|n: WalkNode| {
        let snap_idx = match &n.kind {
            WalkNodeKind::File { source, size, mtime, layer, cache_key } => builder.add_file(
                n.display,
                source,
                *size,
                *mtime,
                layer.0,
                cache_key.0,
            ),
            WalkNodeKind::Dir => {
                let children: Vec<(String, u32)> = n
                    .children
                    .iter()
                    .map(|(folded, child_id)| (folded.clone(), id_map[child_id]))
                    .collect();
                builder.add_dir(n.display, &children)
            }
            WalkNodeKind::Tombstone => builder.add_tombstone(n.display),
        };
        id_map.insert(n.id, snap_idx);
        if n.id == tree.root_id() {
            root_snap = snap_idx;
        }
    });

    builder.set_root(root_snap);
    builder.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};

    fn file(vpath: &str, source: &str, size: u64, mtime: i64) -> InputEntry {
        InputEntry { vpath: vpath.into(), kind: EntryKind::File, source: source.into(), size, mtime }
    }

    #[test]
    fn flatten_produces_readable_snapshot() {
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![file("data/a.esp", "src/a", 10, 1)],
        }])
        .unwrap();
        let img = flatten(&tree);
        let r = crate::reader::SnapshotReader::open(&img).unwrap();
        assert!(matches!(
            r.resolve(&["data", "a.esp"]),
            crate::reader::SnapResolution::File { .. }
        ));
    }

    #[test]
    fn flatten_preserves_tombstones() {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let entry = |vpath: &str, kind: EntryKind| InputEntry {
            vpath: vpath.into(), kind, source: "s".into(), size: 0, mtime: 0,
        };
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![entry("data/keep.esp", EntryKind::File), entry("data/gone.esp", EntryKind::Tombstone)],
        }])
        .unwrap();
        let img = flatten(&tree);
        let r = crate::reader::SnapshotReader::open(&img).unwrap();
        assert_eq!(r.resolve(&["data", "gone.esp"]), crate::reader::SnapResolution::Tombstone);
        assert!(matches!(r.resolve(&["data", "keep.esp"]), crate::reader::SnapResolution::File { .. }));
    }
}
