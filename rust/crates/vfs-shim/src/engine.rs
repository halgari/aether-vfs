//! The redirect engine: a `RootMap` plus the snapshot bytes it resolves against.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::OnceLock;

use vfs_redirect::{classify_open, to_nt, AttrDecision, Decision, DirItem, RootMap, VolumeMap};
use vfs_shared::{LayoutError, SnapshotReader};

use crate::overlay::{Overlay, OverlayState};

thread_local! {
    /// Guards [`Engine::map`]'s lazy `RootMap` resolution against re-entering
    /// itself on the same thread.
    ///
    /// `resolve_volume_map`'s junction scan makes real Win32 calls
    /// (`vfs_win::reparse_point_target`'s `CreateFileW`, directory listings)
    /// while resolving the alias table. Inside an injected process whose own
    /// file APIs are hooked — this crate's whole reason for existing — those
    /// calls are themselves intercepted and fed back through the very same
    /// chain (hook -> `Engine::decide`/`query_attributes` -> `Engine::map`)
    /// on the *same thread*, before the first call's
    /// `OnceLock::get_or_init` closure has returned. `std::sync::Once`
    /// documents same-thread reentrant `call_once` as unspecified behaviour
    /// ("a panic or a deadlock") — verified by reproduction, not assumed:
    /// this exact shape hung the escape-matrix e2e test's injected fixture
    /// process (high, sustained CPU use, not a clean crash — consistent with
    /// undefined behaviour at the FFI boundary a panic there would cross)
    /// before this guard existed. Same failure class, same fix shape, as
    /// `vfs-redirect`'s own `OS_CONSULT_DEPTH`/`OsConsultGuard` for the
    /// unrelated (but structurally identical) 8.3-short-name OS-consult
    /// reentrancy this project already found and fixed once.
    ///
    /// The break: a reentrant call finds the guard already held and
    /// `Engine::map` answers `None` ("not ready yet") instead of touching
    /// the still-initializing `OnceLock` again — every caller already has a
    /// fail-safe `PassThrough`/`false`/`None` fallback for "not resolved",
    /// the same shape used everywhere else in this file for "outside the
    /// root" or "no overlay". The nested Win32 call's own hook invocation
    /// then takes that fall-through path and reaches the real, unhooked
    /// syscall, so `reparse_point_target`'s handle operations still complete
    /// normally — nothing is answered incorrectly, only the nested attempt
    /// to finish initializing `self.map` a second time is skipped.
    static MAP_INIT_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// RAII guard for [`MAP_INIT_DEPTH`]. `enter()` returns `None` when the guard
/// is already held on this thread — the caller's signal to answer "not ready
/// yet" rather than recurse into the still-initializing `RootMap`.
struct MapInitGuard(());

impl MapInitGuard {
    fn enter() -> Option<Self> {
        MAP_INIT_DEPTH.with(|c| {
            if c.get() > 0 {
                None
            } else {
                c.set(1);
                Some(MapInitGuard(()))
            }
        })
    }
}

impl Drop for MapInitGuard {
    fn drop(&mut self) {
        MAP_INIT_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

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
    root: String,
    /// The volume-aware `RootMap`, built lazily — see [`Engine::map`] for why
    /// this is deferred past `build()` rather than eager.
    map: OnceLock<RootMap>,
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
        // Validate the root shape eagerly, so a bad root is still reported
        // from `Engine::new`/`with_overlay` immediately (unchanged observable
        // behaviour) — but with an empty `VolumeMap`, since this call exists
        // only to surface `PathError`, not to build the real, volume-aware
        // map the engine will actually use. See `Engine::map` for why the
        // real one is deferred.
        RootMap::new(root, VolumeMap::empty()).map_err(EngineError::Root)?;
        SnapshotReader::open(&snapshot).map_err(EngineError::Snapshot)?;
        let overlay = overlay_root.map(Overlay::new);
        Ok(Engine { root: root.to_string(), map: OnceLock::new(), snapshot, overlay })
    }

    /// The volume-aware `RootMap`, built on first use and memoized for the
    /// rest of the engine's life — exactly once per session, same cost as
    /// building it eagerly in `build()`, but deliberately *not* eager.
    ///
    /// `resolve_volume_map`'s junction scan (escape-matrix vector 7) reads
    /// the real filesystem at the moment it runs. `build()` runs from the
    /// shim's own DLL bootstrap, which the injector guarantees completes
    /// (and hooks go live) *before* the target's own `main()` executes any
    /// application code — see `vfs-shim-dll`'s module doc. A junction a game
    /// or mod manager already has in place before the game process even
    /// starts is unaffected either way: bootstrap-time and first-decision-
    /// time are the same instant for all practical purposes, both well
    /// before the game does anything with the filesystem. The two moments
    /// only diverge for an artificial case this project's own escape-matrix
    /// fixture happens to construct: a junction created *by the injected
    /// process's own later code*, after bootstrap already ran. Deferring to
    /// first use costs nothing extra in the real-world case (still one
    /// resolution for the session) while no longer silently assuming
    /// injection-time is early enough to see everything that will ever
    /// matter — a strictly more honest reading of "session start" than
    /// "the instant the DLL loads".
    ///
    /// `None` only while the *first-ever* call on this engine is still in
    /// progress and a nested, same-thread call reaches this function again
    /// before that first call returns — see [`MAP_INIT_DEPTH`]. Every
    /// caller treats `None` exactly like "not under any managed root",
    /// which is always a safe answer for an open the shim's own resolution
    /// machinery made about itself, not the game.
    fn map(&self) -> Option<&RootMap> {
        if let Some(m) = self.map.get() {
            return Some(m);
        }
        let _guard = MapInitGuard::enter()?;
        Some(self.map.get_or_init(|| {
            let volumes = vfs_redirect::resolve_volume_map(&self.root);
            RootMap::new(&self.root, volumes)
                .expect("root shape already validated by build()'s empty-VolumeMap check")
        }))
    }

    /// The overlay resolution for `nt_path`, if an overlay is configured, the
    /// engine's `RootMap` is currently available (see [`Engine::map`]), and
    /// the path is under the root.
    fn overlay_state(&self, nt_path: &str) -> Option<OverlayState> {
        let ov = self.overlay.as_ref()?;
        let comps = self.map()?.remainder(nt_path)?;
        Some(ov.lookup(&comps))
    }

    /// Decide how to handle an incoming NT open path. Overlay-first, then
    /// snapshot. Fail-safe: if the snapshot somehow fails to re-open, or the
    /// `RootMap` is not currently available (see [`Engine::map`]), pass
    /// through.
    pub fn decide(&self, nt_path: &str) -> Decision {
        match self.overlay_state(nt_path) {
            Some(OverlayState::Present { path, .. }) => {
                return Decision::Redirect { target_nt: to_nt(&path.to_string_lossy()) }
            }
            Some(OverlayState::Whiteout) => return Decision::Deny,
            Some(OverlayState::Absent) | None => {}
        }
        match (self.map(), SnapshotReader::open(&self.snapshot)) {
            (Some(map), Ok(reader)) => map.decide(nt_path, &reader),
            _ => Decision::PassThrough,
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
        match (self.map(), SnapshotReader::open(&self.snapshot)) {
            (Some(map), Ok(reader)) => map.query_attributes(nt_path, &reader),
            _ => AttrDecision::PassThrough,
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
        let Some(map) = self.map() else { return Decision::PassThrough };
        let comps = match map.remainder(nt_path) {
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
            let dest = ov.file_path(&comps);
            if !self.cow_seed(nt_path, &dest) {
                // Best-effort; write still goes to the overlay path.
            }
        }
        Decision::Redirect { target_nt: to_nt(&ov.file_path(&comps).to_string_lossy()) }
    }

    /// Seed an overlay path with existing content (disk redirect, zip window, or
    /// real file). Returns true when bytes were written to `dest`.
    fn cow_seed(&self, nt_path: &str, dest: &std::path::Path) -> bool {
        if let (Some(map), Ok(reader)) = (self.map(), SnapshotReader::open(&self.snapshot)) {
            match map.decide(nt_path, &reader) {
                Decision::Redirect { target_nt } => {
                    return std::fs::copy(strip_nt(&target_nt), dest).is_ok();
                }
                Decision::Serve {
                    container_nt,
                    offset,
                    length,
                } => {
                    return crate::zipserve::copy_window_to_file(
                        &container_nt,
                        offset,
                        length,
                        dest,
                    );
                }
                _ => {}
            }
        }
        let real = PathBuf::from(strip_nt(nt_path));
        real.exists() && std::fs::copy(&real, dest).is_ok()
    }

    /// Whether `nt_path` lies under the managed root.
    pub fn is_under_root(&self, nt_path: &str) -> bool {
        self.map().is_some_and(|m| m.contains(nt_path))
    }

    /// The folded remainder components of `nt_path` under the root, or `None`.
    pub fn remainder(&self, nt_path: &str) -> Option<Vec<String>> {
        self.map()?.remainder(nt_path)
    }

    /// Whiteout `nt_path` in the overlay (mark as deleted). Returns whether an
    /// overlay handled it; `false` means the caller should let the real delete
    /// proceed (read-only VFS or path outside the root).
    pub fn whiteout(&self, nt_path: &str) -> bool {
        let Some(map) = self.map() else { return false };
        match (&self.overlay, map.remainder(nt_path)) {
            (Some(ov), Some(comps)) if !comps.is_empty() => {
                ov.whiteout(&comps);
                true
            }
            _ => false,
        }
    }

    /// Rename `from_nt` to `to_nt` within the overlay: materialize the source if
    /// needed, move it, and whiteout the old location. Returns whether it was
    /// handled; `false` (no overlay, or either side not cleanly under root) means
    /// the caller should let the real rename proceed.
    pub fn rename(&self, from_nt: &str, to_nt: &str) -> bool {
        let ov = match &self.overlay {
            Some(o) => o,
            None => return false,
        };
        let Some(map) = self.map() else { return false };
        let from = match map.remainder(from_nt) {
            Some(c) if !c.is_empty() => c,
            _ => return false,
        };
        let to = match map.remainder(to_nt) {
            Some(c) if !c.is_empty() => c,
            _ => return false,
        };
        ov.ensure_parent(&from);
        if !ov.has_file(&from) {
            let dest = ov.file_path(&from);
            let _ = self.cow_seed(from_nt, &dest);
        }
        ov.rename(&from, &to);
        true
    }

    /// Merge a directory's real on-disk entries with the snapshot's virtual
    /// children, then apply the overlay (adds/overrides win, whiteouts remove).
    /// Fail-safe: on snapshot re-open failure, or if the `RootMap` is not
    /// currently available (see [`Engine::map`]), returns `real` unchanged.
    pub fn merge_directory(
        &self,
        dir_nt_path: &str,
        real: &[DirItem],
        wildcard: Option<&str>,
    ) -> Vec<DirItem> {
        let map = self.map();
        let merged = match (map, SnapshotReader::open(&self.snapshot)) {
            (Some(m), Ok(reader)) => m.merge_directory(dir_nt_path, &reader, real, wildcard),
            _ => real.to_vec(),
        };
        match (&self.overlay, map.and_then(|m| m.remainder(dir_nt_path))) {
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
