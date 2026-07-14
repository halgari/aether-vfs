//! The redirect engine: a `RootMap` plus the snapshot bytes it resolves against.

use std::path::PathBuf;

use vfs_redirect::{classify_open, to_nt, AttrDecision, Decision, DirItem, RootMap};
use vfs_shared::{LayoutError, SnapshotReader};

use crate::overlay::{Overlay, OverlayState};

/// Strip a `\??\` / `\\?\` device prefix, leaving a Win32 path (drive intact).
fn strip_nt(p: &str) -> String {
    p.strip_prefix(r"\??\").or_else(|| p.strip_prefix(r"\\?\")).unwrap_or(p).to_string()
}

/// Errors constructing an [`Engine`].
#[derive(Debug)]
pub enum EngineError {
    /// The managed root path could not be normalized.
    Root(vfs_core::PathError),
    /// The snapshot bytes failed layout validation.
    Snapshot(LayoutError),
}

/// Owns the redirect policy, the snapshot it resolves against, and an optional
/// write overlay consulted ahead of the snapshot.
pub struct Engine {
    map: RootMap,
    snapshot: Vec<u8>,
    overlay: Option<Overlay>,
}

impl Engine {
    /// `root` may be NT (`\??\C:\Games\Skyrim`) or Win32 (`C:\Games\Skyrim`).
    /// The snapshot is validated eagerly so `decide` can stay infallible.
    /// No write overlay (read-only VFS).
    pub fn new(root: &str, snapshot: Vec<u8>) -> Result<Self, EngineError> {
        Self::build(root, None, snapshot)
    }

    /// Like [`Engine::new`] but with an on-disk write overlay (create/modify/
    /// delete land there; reads resolve overlay-first).
    pub fn with_overlay(
        root: &str,
        overlay_root: &str,
        snapshot: Vec<u8>,
    ) -> Result<Self, EngineError> {
        Self::build(root, Some(overlay_root), snapshot)
    }

    fn build(root: &str, overlay_root: Option<&str>, snapshot: Vec<u8>) -> Result<Self, EngineError> {
        let map = RootMap::new(root).map_err(EngineError::Root)?;
        SnapshotReader::open(&snapshot).map_err(EngineError::Snapshot)?;
        let overlay = overlay_root.map(Overlay::new);
        Ok(Engine { map, snapshot, overlay })
    }

    /// The overlay resolution for `nt_path`, if an overlay is configured and the
    /// path is under the root.
    fn overlay_state(&self, nt_path: &str) -> Option<OverlayState> {
        let ov = self.overlay.as_ref()?;
        let comps = self.map.remainder(nt_path)?;
        Some(ov.lookup(&comps))
    }

    /// Decide how to handle an incoming NT open path. Overlay-first, then
    /// snapshot. Fail-safe: if the snapshot somehow fails to re-open, pass
    /// through (cannot happen after construction).
    pub fn decide(&self, nt_path: &str) -> Decision {
        match self.overlay_state(nt_path) {
            Some(OverlayState::Present { path, .. }) => {
                return Decision::Redirect { target_nt: to_nt(&path.to_string_lossy()) }
            }
            Some(OverlayState::Whiteout) => return Decision::Deny,
            Some(OverlayState::Absent) | None => {}
        }
        match SnapshotReader::open(&self.snapshot) {
            Ok(reader) => self.map.decide(nt_path, &reader),
            Err(_) => Decision::PassThrough,
        }
    }

    /// Answer a path-based attribute query. Overlay-first, then snapshot.
    pub fn query_attributes(&self, nt_path: &str) -> AttrDecision {
        match self.overlay_state(nt_path) {
            Some(OverlayState::Present { is_dir, size, mtime, .. }) => {
                return AttrDecision::Attributes { is_dir, size, mtime }
            }
            Some(OverlayState::Whiteout) => return AttrDecision::Deny,
            Some(OverlayState::Absent) | None => {}
        }
        match SnapshotReader::open(&self.snapshot) {
            Ok(reader) => self.map.query_attributes(nt_path, &reader),
            Err(_) => AttrDecision::PassThrough,
        }
    }

    /// Decide how to handle an open given its desired-access mask and create
    /// disposition. Reads use the overlay-first read resolution; writes (with an
    /// overlay configured) redirect to the overlay, copy-on-write materializing
    /// existing content first. Without an overlay, writes pass through.
    pub fn decide_open(&self, nt_path: &str, access: u32, disposition: u32) -> Decision {
        let intent = classify_open(access, disposition);
        if !intent.write {
            return self.decide(nt_path);
        }
        let ov = match &self.overlay {
            Some(o) => o,
            None => return Decision::PassThrough,
        };
        let comps = match self.map.remainder(nt_path) {
            Some(c) if !c.is_empty() => c,
            _ => return Decision::PassThrough,
        };
        ov.ensure_parent(&comps);
        // Recreating the path: drop any whiteout so it is visible again.
        ov.clear_whiteout(&comps);
        // Copy-on-write: preserve existing content into the overlay before the
        // caller writes, unless it is truncating/replacing (no copy needed) or a
        // copy already exists.
        if intent.preserves && !ov.has_file(&comps) {
            if let Some(src) = self.materialize_source(nt_path) {
                let _ = std::fs::copy(&src, ov.file_path(&comps));
            }
        }
        Decision::Redirect { target_nt: to_nt(&ov.file_path(&comps).to_string_lossy()) }
    }

    /// The current on-disk source of `nt_path`'s content to seed a COW copy:
    /// the snapshot's mod backing if virtual, else the real file if it exists.
    fn materialize_source(&self, nt_path: &str) -> Option<PathBuf> {
        if let Ok(reader) = SnapshotReader::open(&self.snapshot) {
            if let Decision::Redirect { target_nt } = self.map.decide(nt_path, &reader) {
                return Some(PathBuf::from(strip_nt(&target_nt)));
            }
        }
        let real = PathBuf::from(strip_nt(nt_path));
        real.exists().then_some(real)
    }

    /// Whether `nt_path` lies under the managed root.
    pub fn is_under_root(&self, nt_path: &str) -> bool {
        self.map.contains(nt_path)
    }

    /// The folded remainder components of `nt_path` under the root, or `None`.
    pub fn remainder(&self, nt_path: &str) -> Option<Vec<String>> {
        self.map.remainder(nt_path)
    }

    /// Whiteout `comps` in the overlay (mark as deleted). Returns whether an
    /// overlay is configured to handle it; `false` means the caller should let
    /// the real delete proceed (read-only VFS).
    pub fn whiteout(&self, comps: &[String]) -> bool {
        match &self.overlay {
            Some(ov) => {
                ov.whiteout(comps);
                true
            }
            None => false,
        }
    }

    /// Merge a directory's real on-disk entries with the snapshot's virtual
    /// children, then apply the overlay (adds/overrides win, whiteouts remove).
    /// Fail-safe: on snapshot re-open failure, returns `real` unchanged.
    pub fn merge_directory(
        &self,
        dir_nt_path: &str,
        real: &[DirItem],
        wildcard: Option<&str>,
    ) -> Vec<DirItem> {
        let merged = match SnapshotReader::open(&self.snapshot) {
            Ok(reader) => self.map.merge_directory(dir_nt_path, &reader, real, wildcard),
            Err(_) => real.to_vec(),
        };
        match (&self.overlay, self.map.remainder(dir_nt_path)) {
            (Some(ov), Some(comps)) => ov.apply_to_listing(&comps, merged, wildcard),
            _ => merged,
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

    // Overlay-aware resolution (W1). Uses a real temp overlay directory.
    fn overlay_engine(overlay_dir: &std::path::Path) -> Engine {
        Engine::with_overlay(
            r"\??\C:\Games\Skyrim",
            overlay_dir.to_str().unwrap(),
            snapshot_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn overlay_file_wins_over_snapshot() {
        let dir = std::env::temp_dir().join(format!("vfs-ovl-win-{}", std::process::id()));
        // Overlay copy of data/foo.esp (folded components on disk).
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(dir.join("data").join("foo.esp"), b"overlaid").unwrap();
        let engine = overlay_engine(&dir);
        let d = engine.decide(r"\??\C:\Games\Skyrim\Data\foo.esp");
        match d {
            Decision::Redirect { target_nt } => {
                assert!(target_nt.to_lowercase().contains("vfs-ovl-win"), "{target_nt}");
                assert!(target_nt.starts_with(r"\??\"));
            }
            other => panic!("expected overlay redirect, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlay_whiteout_hides_snapshot_file() {
        use vfs_redirect::AttrDecision;
        let dir = std::env::temp_dir().join(format!("vfs-ovl-wh-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        // Whiteout marker for data/foo.esp.
        std::fs::write(dir.join("data").join("foo.esp.__vfs_wh__"), b"").unwrap();
        let engine = overlay_engine(&dir);
        assert_eq!(engine.decide(r"\??\C:\Games\Skyrim\Data\foo.esp"), Decision::Deny);
        assert_eq!(
            engine.query_attributes(r"\??\C:\Games\Skyrim\Data\foo.esp"),
            AttrDecision::Deny
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlay_absent_falls_through_to_snapshot() {
        let dir = std::env::temp_dir().join(format!("vfs-ovl-abs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let engine = overlay_engine(&dir);
        // No overlay entry -> snapshot redirect to the backing file.
        assert_eq!(
            engine.decide(r"\??\C:\Games\Skyrim\Data\foo.esp"),
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
