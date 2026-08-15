//! The redirect engine: a `RootMap` plus the snapshot bytes it resolves against.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::OnceLock;

use vfs_redirect::{classify_open, to_nt, Decision, DirItem, RootMap, VolumeMap};
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
    /// chain (hook -> `Engine::decide`/`overlay_state` -> `Engine::map`)
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
    ///
    /// `pub(crate)` (rather than private): since Task 4 deleted
    /// `RootMap::query_attributes` and `AttrDecision` — the local
    /// snapshot-backed attribute answering this crate used to fall back to —
    /// the shim-local write overlay is the only thing left that can still
    /// answer an attribute query without asking the director. `hook.rs`'s
    /// `qattr_hook`/`qfull_hook`/`qibn_hook` call this directly for exactly
    /// that: a file just created/modified through the overlay write
    /// fallback (gate 4's mechanism, untouched by Task 4) must still report
    /// sane attributes even though the director has never heard of it.
    pub(crate) fn overlay_state(&self, nt_path: &str) -> Option<OverlayState> {
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

    /// Apply the shim-local write overlay (adds/overrides win, whiteouts
    /// remove) on top of a directory's real on-disk entries.
    ///
    /// Task 4 deleted `RootMap::merge_directory`, which used to blend the
    /// published snapshot's virtual children into `real` here — a directory
    /// listing under a managed root now comes solely from the director's own
    /// `readdir` (see `hook.rs::serve_dir_query`), never from a local
    /// snapshot merge. This method keeps only the overlay half: the write
    /// fallback (gate 4's mechanism, explicitly out of scope for this task)
    /// still needs a just-created/modified/deleted overlay entry to show up
    /// in a listing the director cannot itself account for. Fail-safe: no
    /// overlay configured, or the `RootMap` not currently available (see
    /// [`Engine::map`]), returns `real` unchanged.
    pub fn overlay_listing(
        &self,
        dir_nt_path: &str,
        real: &[DirItem],
        wildcard: Option<&str>,
    ) -> Vec<DirItem> {
        match (&self.overlay, self.map().and_then(|m| m.remainder(dir_nt_path))) {
            (Some(ov), Some(comps)) => ov.apply_to_listing(&comps, real.to_vec(), wildcard),
            _ => real.to_vec(),
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

    /// Gate 3, Task 5's Step 1: the failing test written first. A REAL file,
    /// physically on disk under a real managed root, that no provider serves
    /// (the snapshot has an entry for a completely different vpath, so this
    /// file is not even a `Dir` node in the tree) must be unreachable through
    /// `Engine::decide` -- before this task's fix this returned
    /// `Decision::PassThrough`, and `hook::create_hook`/`open_hook` trampoline
    /// on exactly that verdict, opening the real bytes below.
    #[test]
    fn real_on_disk_file_under_root_not_in_snapshot_is_denied() {
        let dir = std::env::temp_dir()
            .join(format!("vfs-engine-negcanary-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Data")).unwrap();
        let real_file = dir.join("Data").join("negative-canary.bin");
        std::fs::write(&real_file, b"the real bytes physically on disk").unwrap();
        assert!(real_file.is_file(), "setup: the real file must actually exist");

        let engine = Engine::new(&dir.to_string_lossy(), snapshot_bytes()).unwrap();
        let path = format!(r"\??\{}", real_file.to_string_lossy());
        assert_eq!(
            engine.decide(&path),
            Decision::Deny,
            "a real, on-disk file under the root with no provider must be denied, not opened"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Gate 3's own predicted consequence (gate 2's final review; see
    /// `rust/docs/escape-matrix.md`'s "Mod Organizer exposure" note): a
    /// junction *inside* the managed root pointing at external staging
    /// content is a common real-world Skyrim/MO2 layout. Verified here with a
    /// REAL junction (`mklink /J`), not merely reasoned about: the junction
    /// makes the target's bytes genuinely, transparently reachable by
    /// `std::fs` (proving the content really is there and the OS really does
    /// resolve it), while `Engine::decide` -- operating on the same literal,
    /// unresolved path string a real hooked open would present, exactly as
    /// `RootMap::compute_under_root` does not follow junctions -- now denies
    /// it. Before this task's fix this would have been `PassThrough`, and the
    /// junction's transparency at the kernel level is exactly why that used
    /// to work: the shim never needed to resolve the junction itself, because
    /// passing through let the OS do it. Removing the passthrough seals that
    /// content along with everything else `NotFound` covers, which is the gate
    /// 2 review's prediction confirmed by reproduction rather than assumed.
    #[test]
    #[cfg(windows)]
    fn mo2_style_junction_inside_root_pointing_to_external_staging_is_sealed() {
        let base = std::env::temp_dir()
            .join(format!("vfs-engine-mo2-junction-{}", std::process::id()));
        let root_dir = base.join("root");
        // Deliberately NOT under root, and NOT mounted as a provider anywhere
        // -- the external mod-staging directory an MO2-style setup points at.
        let staging_dir = base.join("staging");
        std::fs::create_dir_all(root_dir.join("Data")).unwrap();
        std::fs::create_dir_all(&staging_dir).unwrap();
        const BYTES: &[u8] = b"the mo2-staged mod's real bytes";
        std::fs::write(staging_dir.join("mo2-mod.esp"), BYTES).unwrap();

        let junction = root_dir.join("Data").join("SomeMod");
        let ok = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &junction.to_string_lossy(),
                &staging_dir.to_string_lossy(),
            ])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            // mklink unavailable/needs a privilege this account lacks: nothing
            // to test here, same convention as this crate's own 8.3 tests.
            std::fs::remove_dir_all(&base).ok();
            return;
        }

        let via_junction = junction.join("mo2-mod.esp");
        // Proves the junction is real and the OS really does resolve it
        // transparently -- the exact mechanism the old passthrough relied on.
        assert_eq!(
            std::fs::read(&via_junction).unwrap(),
            BYTES,
            "setup: the junction must transparently resolve to the staging dir's real bytes"
        );

        let engine = Engine::new(&root_dir.to_string_lossy(), snapshot_bytes()).unwrap();
        let path = format!(r"\??\{}", via_junction.to_string_lossy());
        assert_eq!(
            engine.decide(&path),
            Decision::Deny,
            "an MO2-style junction's real, externally-staged content must now be sealed \
             -- mounting the staging directory as a provider is the required fix, not \
             restoring the passthrough (see rust/docs/escape-matrix.md)"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // Task 4 removed `merge_directory_adds_virtual_children` and
    // `query_attributes_reports_virtual_file`: both asserted that a plain,
    // no-overlay `Engine` answers a directory listing / attribute query from
    // its local snapshot alone (no director involved). That is exactly the
    // local-answering path Task 4 deletes — `RootMap::merge_directory` and
    // `RootMap::query_attributes` no longer exist for `Engine` to call.
    // A directory listing under a managed root is now the director's
    // `readdir` alone (`hook.rs::serve_dir_query`); an attribute query is the
    // director's `getattr` alone (`hook.rs::fuse_path_attr`). Neither is
    // reachable from this crate's fast, no-director unit tests without a
    // live ring, so there is no in-crate equivalent to port these two to —
    // see the task report for this named gap. `overlay_listing_adds_overlay_children`
    // below replaces the still-relevant half: the shim-local write overlay
    // (gate 4's mechanism, unaffected by Task 4) must still contribute to a
    // listing regardless of what answers the rest of it.
    #[test]
    fn overlay_listing_adds_overlay_children() {
        use vfs_redirect::DirItem;
        let dir = std::env::temp_dir().join(format!("vfs-ovl-listing-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::write(dir.join("data").join("overlaid.txt"), b"x").unwrap();
        let engine = overlay_engine(&dir);
        let real = vec![DirItem { name: "real.txt".into(), is_dir: false, size: 1, mtime: 0 }];
        let listed = engine.overlay_listing(r"\??\C:\Games\Skyrim\Data", &real, None);
        let names: Vec<&str> = listed.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"real.txt"), "real entry dropped: {names:?}");
        assert!(names.contains(&"overlaid.txt"), "overlay entry missing: {names:?}");
        // The snapshot's "foo.esp" must NOT appear: no director, no overlay
        // entry for it -- nothing answers for it anymore.
        assert!(!names.contains(&"foo.esp"), "snapshot leaked into the listing: {names:?}");
        let _ = std::fs::remove_dir_all(&dir);
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
        use crate::overlay::OverlayState;
        let dir = std::env::temp_dir().join(format!("vfs-ovl-wh-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        // Whiteout marker for data/foo.esp.
        std::fs::write(dir.join("data").join("foo.esp.__vfs_wh__"), b"").unwrap();
        let engine = overlay_engine(&dir);
        assert_eq!(engine.decide(r"\??\C:\Games\Skyrim\Data\foo.esp"), Decision::Deny);
        // `AttrDecision`/`Engine::query_attributes` are gone (Task 4); the
        // overlay's own whiteout state is what a caller now consults for an
        // attribute-query fallback (see `hook.rs`'s qattr/qfull/qibn hooks).
        assert!(matches!(
            engine.overlay_state(r"\??\C:\Games\Skyrim\Data\foo.esp"),
            Some(OverlayState::Whiteout)
        ));
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
