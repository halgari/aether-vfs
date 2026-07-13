//! The redirect engine: a `RootMap` plus the snapshot bytes it resolves against.

use vfs_redirect::{AttrDecision, Decision, DirItem, RootMap};
use vfs_shared::{LayoutError, SnapshotReader};

/// Errors constructing an [`Engine`].
#[derive(Debug)]
pub enum EngineError {
    /// The managed root path could not be normalized.
    Root(vfs_core::PathError),
    /// The snapshot bytes failed layout validation.
    Snapshot(LayoutError),
}

/// Owns the redirect policy and the snapshot it resolves against.
pub struct Engine {
    map: RootMap,
    snapshot: Vec<u8>,
}

impl Engine {
    /// `root` may be NT (`\??\C:\Games\Skyrim`) or Win32 (`C:\Games\Skyrim`).
    /// The snapshot is validated eagerly so `decide` can stay infallible.
    pub fn new(root: &str, snapshot: Vec<u8>) -> Result<Self, EngineError> {
        let map = RootMap::new(root).map_err(EngineError::Root)?;
        SnapshotReader::open(&snapshot).map_err(EngineError::Snapshot)?;
        Ok(Engine { map, snapshot })
    }

    /// Decide how to handle an incoming NT open path. Fail-safe: if the snapshot
    /// somehow fails to re-open, pass through (cannot happen after `new`).
    pub fn decide(&self, nt_path: &str) -> Decision {
        match SnapshotReader::open(&self.snapshot) {
            Ok(reader) => self.map.decide(nt_path, &reader),
            Err(_) => Decision::PassThrough,
        }
    }

    /// Answer a path-based attribute query against the snapshot. Fail-safe.
    pub fn query_attributes(&self, nt_path: &str) -> AttrDecision {
        match SnapshotReader::open(&self.snapshot) {
            Ok(reader) => self.map.query_attributes(nt_path, &reader),
            Err(_) => AttrDecision::PassThrough,
        }
    }

    /// Whether `nt_path` lies under the managed root.
    pub fn is_under_root(&self, nt_path: &str) -> bool {
        self.map.contains(nt_path)
    }

    /// Merge a directory's real on-disk entries with the snapshot's virtual
    /// children. Fail-safe: on snapshot re-open failure, returns `real`
    /// unchanged (never hides real files on error).
    pub fn merge_directory(
        &self,
        dir_nt_path: &str,
        real: &[DirItem],
        wildcard: Option<&str>,
    ) -> Vec<DirItem> {
        match SnapshotReader::open(&self.snapshot) {
            Ok(reader) => self.map.merge_directory(dir_nt_path, &reader, real, wildcard),
            Err(_) => real.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_redirect::Decision;

    fn snapshot_bytes() -> Vec<u8> {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/foo.esp".into(),
                kind: EntryKind::File,
                source: r"D:\Mods\Cool\foo.esp".into(),
                size: 10,
                mtime: 1,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    }

    #[test]
    fn new_rejects_a_bad_snapshot() {
        // Use `matches!` on the whole Result rather than `.unwrap_err()` — the
        // latter needs `Engine: Debug`, but Engine holds a `Vec<u8>` snapshot we
        // don't want dumped, so Engine intentionally does not derive Debug.
        assert!(matches!(
            Engine::new(r"C:\Games\Skyrim", vec![0u8; 4]),
            Err(EngineError::Snapshot(_))
        ));
    }

    #[test]
    fn decide_redirects_a_virtual_file() {
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        let d = engine.decide(r"\??\C:\Games\Skyrim\Data\foo.esp");
        assert_eq!(d, Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() });
    }

    #[test]
    fn decide_passes_through_outside_root() {
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        assert_eq!(engine.decide(r"\??\C:\Windows\notepad.exe"), Decision::PassThrough);
    }

    #[test]
    fn is_under_root_predicate() {
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        assert!(engine.is_under_root(r"\??\C:\Games\Skyrim\Data\foo.esp"));
        assert!(!engine.is_under_root(r"\??\C:\Windows\notepad.exe"));
    }

    #[test]
    fn merge_directory_adds_virtual_children() {
        use vfs_redirect::DirItem;
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        let real = vec![DirItem { name: "real.txt".into(), is_dir: false, size: 1, mtime: 0 }];
        let merged = engine.merge_directory(r"\??\C:\Games\Skyrim\Data", &real, None);
        let names: Vec<&str> = merged.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"real.txt"));
        assert!(names.contains(&"foo.esp"));
    }

    #[test]
    fn query_attributes_reports_virtual_file() {
        use vfs_redirect::AttrDecision;
        let engine = Engine::new(r"\??\C:\Games\Skyrim", snapshot_bytes()).unwrap();
        assert_eq!(
            engine.query_attributes(r"\??\C:\Games\Skyrim\Data\foo.esp"),
            AttrDecision::Attributes { is_dir: false, size: 10, mtime: 1 }
        );
    }
}
