//! The redirect engine: a `RootMap` plus the snapshot bytes it resolves against.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::OnceLock;

use vfs_redirect::{classify_open, to_nt, Decision, DirItem, RootId, RootMap, VolumeMap};
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
    /// Every managed root this session declared, as `(id, path)`.
    ///
    /// **Several, not one** (gate 4, Task 3). The rest of the stack —
    /// `RootMap`, the director, the ring's `root:u32`, the block cache, this
    /// crate's own `Overlay` — has been root-addressed since stage 2b; this
    /// was the last single-root thing, and it is the one that decides what
    /// happens when the director does *not* answer. While it knew only root
    /// 0, a path under root ≥1 was `Outside` to it, so a write that missed
    /// the director landed on real disk with no redirect and no deny.
    roots: Vec<(RootId, String)>,
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
    ///
    /// Single-root shorthand for [`Engine::with_roots`] — the shape most
    /// tests and every pre-stage-2b caller want.
    pub fn new(root: &str, snapshot: Vec<u8>) -> Result<Self, EngineError> {
        Self::build(&[(RootId::DEFAULT, root.to_string())], None, snapshot)
    }

    /// Like [`Engine::new`] but with an on-disk write overlay (create/modify/
    /// delete land there; reads resolve overlay-first).
    pub fn with_overlay(
        root: &str,
        overlay_root: &str,
        snapshot: Vec<u8>,
    ) -> Result<Self, EngineError> {
        Self::build(&[(RootId::DEFAULT, root.to_string())], Some(overlay_root), snapshot)
    }

    /// Every root the session declared, `(id, path)`. Two entries may share an
    /// id to declare an alias, exactly as [`RootMap::with_roots`] defines it.
    ///
    /// `bootstrap.rs` builds this list with the *same* function the FUSE
    /// client uses (`fuse_client::roots_from_env`), so the two halves of the
    /// shim cannot disagree about which roots exist.
    pub fn with_roots(roots: &[(RootId, String)], snapshot: Vec<u8>) -> Result<Self, EngineError> {
        Self::build(roots, None, snapshot)
    }

    /// [`Engine::with_roots`] plus an on-disk write overlay. The overlay is
    /// root-scoped (`overlay_layer_dir`), so each root's writes land in their
    /// own subtree and identical relative paths under different roots never
    /// share a file.
    pub fn with_roots_and_overlay(
        roots: &[(RootId, String)],
        overlay_root: &str,
        snapshot: Vec<u8>,
    ) -> Result<Self, EngineError> {
        Self::build(roots, Some(overlay_root), snapshot)
    }

    fn build(
        roots: &[(RootId, String)],
        overlay_root: Option<&str>,
        snapshot: Vec<u8>,
    ) -> Result<Self, EngineError> {
        // An engine with no roots would classify every path as `Outside` and
        // therefore virtualize nothing at all — the "content simply missing"
        // failure this project keeps rediscovering, but total. `RootMap` will
        // happily build an empty map, so reject it here.
        if roots.is_empty() {
            return Err(EngineError::Root(vfs_core::PathError::EmptyRoot));
        }
        // Validate the root shapes eagerly, so a bad root is still reported
        // from the constructor immediately (unchanged observable behaviour)
        // — but with an empty `VolumeMap`, since this call exists only to
        // surface `PathError`, not to build the real, volume-aware map the
        // engine will actually use. See `Engine::map` for why the real one is
        // deferred. `with_roots` fails on the first path that will not
        // normalize rather than dropping it, so a second root that is
        // malformed is reported here rather than going quietly missing.
        RootMap::with_roots(&Self::refs(roots), VolumeMap::empty()).map_err(EngineError::Root)?;
        SnapshotReader::open(&snapshot).map_err(EngineError::Snapshot)?;
        let overlay = overlay_root.map(Overlay::new);
        Ok(Engine { roots: roots.to_vec(), map: OnceLock::new(), snapshot, overlay })
    }

    /// Borrowed view of `roots` in the shape `RootMap::with_roots` takes.
    fn refs(roots: &[(RootId, String)]) -> Vec<(RootId, &str)> {
        roots.iter().map(|(id, p)| (*id, p.as_str())).collect()
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
            // Scoped to *every* declared root, not just root 0: a junction or
            // volume-GUID spelling under the second root needs the same alias
            // table the first one does. `FuseClient::connect` scans the same
            // way for the same reason.
            let scan: Vec<&str> = self.roots.iter().map(|(_, p)| p.as_str()).collect();
            let volumes = vfs_redirect::resolve_volume_map_for(&scan);
            RootMap::with_roots(&Self::refs(&self.roots), volumes)
                .expect("root shapes already validated by build()'s empty-VolumeMap check")
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
        let (root, comps) = self.resolve(nt_path)?;
        Some(ov.lookup(root, &comps))
    }

    /// Which declared root `nt_path` falls under, plus its folded remainder
    /// beneath that root — `None` when it is outside every root, malformed,
    /// escaping, or the `RootMap` is not yet available (see [`Engine::map`]).
    ///
    /// **Every root-addressed entry point goes through here**, so the id an
    /// overlay call receives is always the one the path actually resolved
    /// under. That matters more than it looks: an overlay call left at
    /// `RootId::DEFAULT` compiles, passes every single-root test, and quietly
    /// files root 1's writes under root 0 — the collision `Overlay` was made
    /// root-scoped to prevent. `RootMap::remainder`, which throws the id away,
    /// is deliberately not used anywhere in this file for that reason.
    fn resolve(&self, nt_path: &str) -> Option<vfs_redirect::RootHit> {
        self.map()?.resolve(nt_path)
    }

    /// Decide how to handle an incoming NT open path. Overlay-first, then
    /// snapshot. Fail-safe: if the snapshot somehow fails to re-open, or the
    /// `RootMap` is not currently available (see [`Engine::map`]), pass
    /// through.
    ///
    /// **The snapshot answers for root 0 only.** It is one flat vpath tree
    /// with no root dimension — the composition published for the managed
    /// game directory — so resolving root 1's remainder against it would
    /// answer `<root1>\Data\foo.esp` with root *0*'s backing file. That is a
    /// worse failure than the pass-through this became multi-root to remove:
    /// serving one root's bytes under another root's name. So a read under
    /// root ≥1 that the overlay does not answer is sealed (`Deny`) — the same
    /// verdict gate 3 gives any under-root path the provider graph cannot
    /// vouch for, and the same one root 0 gets from the empty snapshot a
    /// director session actually ships (`Session::serve` writes
    /// `empty_tree_snapshot()`; the director's ring, not this snapshot, is
    /// what answers reads in a live session).
    pub fn decide(&self, nt_path: &str) -> Decision {
        match self.overlay_state(nt_path) {
            Some(OverlayState::Present { path, .. }) => {
                return Decision::Redirect { target_nt: to_nt(&path.to_string_lossy()) }
            }
            Some(OverlayState::Whiteout) => return Decision::Deny,
            Some(OverlayState::Absent) | None => {}
        }
        let Some(map) = self.map() else { return Decision::PassThrough };
        match map.resolve(nt_path) {
            // Outside every declared root: never ours to touch.
            None => Decision::PassThrough,
            Some((root, _)) if root != RootId::DEFAULT => Decision::Deny,
            Some(_) => match SnapshotReader::open(&self.snapshot) {
                Ok(reader) => map.decide(nt_path, &reader),
                Err(_) => Decision::PassThrough,
            },
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
        let (root, comps) = match self.resolve(nt_path) {
            Some((r, c)) if !c.is_empty() => (r, c),
            _ => return Decision::PassThrough,
        };
        ov.ensure_parent(root, &comps);
        // Recreating the path: drop any whiteout so it is visible again.
        ov.clear_whiteout(root, &comps);
        // Copy-on-write: preserve existing content into the overlay before the
        // caller writes, unless it is truncating/replacing (no copy needed) or a
        // copy already exists.
        if intent.preserves && !ov.has_file(root, &comps) {
            let dest = ov.file_path(root, &comps);
            if !self.cow_seed(root, nt_path, &dest) {
                // Best-effort; write still goes to the overlay path.
            }
        }
        Decision::Redirect {
            target_nt: to_nt(&ov.file_path(root, &comps).to_string_lossy()),
        }
    }

    /// Seed an overlay path with existing content (disk redirect, zip window, or
    /// real file). Returns true when bytes were written to `dest`.
    ///
    /// `root` is the root `nt_path` resolved under, and the snapshot is only
    /// consulted for root 0 — same reason [`Engine::decide`] gates it there:
    /// seeding root 1's copy-on-write file from root 0's snapshot entry for
    /// the same relative path would bake another root's bytes into the file
    /// the game is about to edit. Root ≥1 falls straight to the real-file
    /// copy below, which is root-correct by construction (it copies the very
    /// path being opened).
    ///
    /// Worth knowing about that fallback rather than assuming it: inside an
    /// injected process the `std::fs::copy` below is itself hooked, and its
    /// source open is decided by `Engine::decide` like any other — so under a
    /// managed root it only ever seeds from content the VFS already vouches
    /// for. A bare real file under root 0 has been invisible since gate 3
    /// sealed the root, and a bare real file under root ≥1 is sealed the same
    /// way now, so in-process the fallback is reached but declines. Out of
    /// process (unit tests, any non-injected caller) it copies as written.
    /// That is the intended shape, not an accident: a preserving write to a
    /// path the VFS says does not exist gets a fresh empty overlay file,
    /// consistent with the not-found the same path reads as. Seeding it from
    /// what the *director* holds is the write fall-through redesign, which is
    /// a separate task.
    fn cow_seed(&self, root: RootId, nt_path: &str, dest: &std::path::Path) -> bool {
        if let (RootId::DEFAULT, Some(map), Ok(reader)) =
            (root, self.map(), SnapshotReader::open(&self.snapshot))
        {
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

    /// Whether `nt_path` lies under **any** declared root. `hook.rs`'s
    /// `path_is_ours` is the only caller; see its doc comment for the one
    /// spelling this still answers `false` for that the FUSE client answers
    /// `true` for (the staged-launch alias).
    pub fn is_under_root(&self, nt_path: &str) -> bool {
        self.map().is_some_and(|m| m.contains(nt_path))
    }

    // `pub fn remainder(&self, nt_path) -> Option<Vec<String>>` used to sit
    // here: the remainder with the `RootId` thrown away. It had no callers
    // left, and now that every overlay operation is addressed by
    // `(RootId, comps)` it is a trap rather than a convenience — a future
    // caller reaching for it would be reaching for exactly the root-blind
    // shape this task removed from the four call sites that had it. Ask
    // `RootMap::resolve` (or this engine's own `resolve`) instead, which
    // hands back the id with the components.

    /// Whiteout `nt_path` in the overlay (mark as deleted). Returns whether an
    /// overlay handled it; `false` means the caller should let the real delete
    /// proceed (read-only VFS or path outside the root).
    pub fn whiteout(&self, nt_path: &str) -> bool {
        match (&self.overlay, self.resolve(nt_path)) {
            (Some(ov), Some((root, comps))) if !comps.is_empty() => {
                ov.whiteout(root, &comps);
                true
            }
            _ => false,
        }
    }

    /// Rename `from_nt` to `to_nt` within the overlay: materialize the source if
    /// needed, move it, and whiteout the old location. Returns whether it was
    /// handled; `false` (no overlay, or either side not cleanly under root) means
    /// the caller should let the real rename proceed.
    ///
    /// A rename whose two sides land under *different* roots is declined
    /// rather than guessed at, matching what `hook.rs` already does one layer
    /// up when the FUSE client answers the same question (`Some((dst_root,
    /// dstv)) if dst_root == root`): `Overlay::rename` moves within one
    /// root's subtree, and there is no cross-root move in the provider
    /// contract either. Picking one of the two ids would file the result
    /// under a root that only half the operation named.
    pub fn rename(&self, from_nt: &str, to_nt: &str) -> bool {
        let ov = match &self.overlay {
            Some(o) => o,
            None => return false,
        };
        let (from_root, from) = match self.resolve(from_nt) {
            Some((r, c)) if !c.is_empty() => (r, c),
            _ => return false,
        };
        let (to_root, to) = match self.resolve(to_nt) {
            Some((r, c)) if !c.is_empty() => (r, c),
            _ => return false,
        };
        if from_root != to_root {
            return false;
        }
        ov.ensure_parent(from_root, &from);
        if !ov.has_file(from_root, &from) {
            let dest = ov.file_path(from_root, &from);
            let _ = self.cow_seed(from_root, from_nt, &dest);
        }
        ov.rename(from_root, &from, &to);
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
        match (&self.overlay, self.resolve(dir_nt_path)) {
            (Some(ov), Some((root, comps))) => {
                ov.apply_to_listing(root, &comps, real.to_vec(), wildcard)
            }
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

    /// A root list that declares nothing would make every path `Outside` and
    /// virtualize precisely nothing — the total form of the "content simply
    /// missing" failure. `RootMap` builds an empty map happily, so `Engine`
    /// rejects it at construction.
    #[test]
    fn with_roots_rejects_an_empty_root_list() {
        assert!(matches!(
            Engine::with_roots(&[], snapshot_bytes()),
            Err(EngineError::Root(vfs_core::PathError::EmptyRoot))
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
        // Root-scoped on-disk layout (see `Overlay::root_dir`): this engine
        // declares one root, so everything it resolves is root 0's.
        std::fs::create_dir_all(dir.join("root-0").join("data")).unwrap();
        std::fs::write(dir.join("root-0").join("data").join("overlaid.txt"), b"x").unwrap();
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
        // Overlay copy of data/foo.esp (folded components on disk), under
        // root 0's subdirectory (see `Overlay::root_dir`) — `overlay_engine`
        // declares one root, so root 0 is the only one it can resolve to.
        std::fs::create_dir_all(dir.join("root-0").join("data")).unwrap();
        std::fs::write(dir.join("root-0").join("data").join("foo.esp"), b"overlaid").unwrap();
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
        // Root 0's subdirectory (see `Overlay::root_dir`) — `overlay_engine`
        // declares one root, so root 0 is the only one it can resolve to.
        std::fs::create_dir_all(dir.join("root-0").join("data")).unwrap();
        // Whiteout marker for data/foo.esp.
        std::fs::write(dir.join("root-0").join("data").join("foo.esp.__vfs_wh__"), b"").unwrap();
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

    /// The gap `a_write_under_a_second_root_passes_through_to_real_disk_today`
    /// recorded, now closed. That test asserted the opposite of this one on
    /// purpose ("when `Engine` becomes multi-root this fails — that is the
    /// point"), and this is the rewrite it asked for.
    ///
    /// Three things have to hold at once, and the contrast between them is
    /// the whole test — asserting root 1 alone could not tell "both roots
    /// handled correctly" from "both roots broken the same way":
    ///
    /// 1. A write under root 0 is captured by the overlay (unchanged).
    /// 2. The identical write under root 1 is captured too — no `PassThrough`
    ///    to real disk, which is the hole this closes.
    /// 3. The two land in *different* overlay files. A `RootMap` that learned
    ///    root 1 but an overlay call still hardcoding `RootId::DEFAULT` would
    ///    satisfy (1) and (2) and still be wrong: both roots' `Data\w.ini`
    ///    would collide on one file (the collision `Overlay` was made
    ///    root-scoped to prevent).
    ///
    /// The fourth assertion is the control: an engine told about root 0 only
    /// still passes root 1 through, because an *undeclared* root genuinely is
    /// outside — `Session::declare_root`'s "mount without declaring and the
    /// shim never classifies any path into that root at all". Declaring is
    /// what changes the answer here, not guessing.
    #[test]
    fn a_write_under_a_second_root_lands_in_that_root_s_overlay() {
        use crate::overlay_layer_dir;
        use vfs_redirect::{FILE_OPEN_IF, FILE_OVERWRITE_IF};
        // GENERIC_WRITE — `classify_open` reads this as a write intent.
        const WRITE: u32 = 0x4000_0000;

        let base = std::env::temp_dir().join(format!("vfs-engine-2root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root0 = base.join("root0");
        // A second root a multi-root session declares. Deliberately not under
        // `root0`: it is a separate location, the way `Documents\My Games\…`
        // is (`skyrim-live` declares exactly that as root 1).
        let root1 = base.join("root1");
        let overlay_dir = base.join("overlay");
        std::fs::create_dir_all(root0.join("Data")).unwrap();
        std::fs::create_dir_all(root1.join("Data")).unwrap();
        std::fs::create_dir_all(&overlay_dir).unwrap();

        let engine = Engine::with_roots_and_overlay(
            &[
                (RootId::DEFAULT, root0.to_string_lossy().into_owned()),
                (RootId(1), root1.to_string_lossy().into_owned()),
            ],
            overlay_dir.to_str().unwrap(),
            snapshot_bytes(),
        )
        .unwrap();

        // The overlay file each write must land in — from `overlay_layer_dir`,
        // the one place the on-disk naming scheme is defined, not spelled out
        // here a second time.
        let expect_target = |root: RootId| {
            to_nt(
                &overlay_layer_dir(&overlay_dir, root)
                    .join("data")
                    .join("w.ini")
                    .to_string_lossy(),
            )
        };

        let under_root0 = format!(r"\??\{}", root0.join("Data").join("w.ini").to_string_lossy());
        assert_eq!(
            engine.decide_open(&under_root0, WRITE, FILE_OVERWRITE_IF),
            Decision::Redirect { target_nt: expect_target(RootId::DEFAULT) },
            "a write under root 0 must land in root 0's overlay subtree"
        );

        let under_root1 = format!(r"\??\{}", root1.join("Data").join("w.ini").to_string_lossy());
        assert_eq!(
            engine.decide_open(&under_root1, WRITE, FILE_OVERWRITE_IF),
            Decision::Redirect { target_nt: expect_target(RootId(1)) },
            "a write under root 1 must land in ROOT 1's overlay subtree — a \
             PassThrough here is the closed gap reopening (the write reaching \
             real disk); a root-0 target here is the same write colliding with \
             root 0's copy of the same relative path"
        );
        assert_eq!(
            engine.decide_open(&under_root1, WRITE, FILE_OPEN_IF),
            Decision::Redirect { target_nt: expect_target(RootId(1)) },
            "same for the copy-on-write disposition"
        );
        assert_ne!(
            expect_target(RootId::DEFAULT),
            expect_target(RootId(1)),
            "setup: the two roots' overlay targets must be distinct paths for \
             the assertions above to mean anything"
        );

        assert!(engine.is_under_root(&under_root0));
        assert!(
            engine.is_under_root(&under_root1),
            "a declared second root is under root as far as this engine is concerned"
        );

        // Control: the same second-root write against an engine that was never
        // told about root 1 still passes through — being outside every
        // declared root is not the bug, never declaring the root is.
        let single = Engine::with_overlay(
            &root0.to_string_lossy(),
            overlay_dir.to_str().unwrap(),
            snapshot_bytes(),
        )
        .unwrap();
        assert_eq!(
            single.decide_open(&under_root1, WRITE, FILE_OVERWRITE_IF),
            Decision::PassThrough,
            "an undeclared root is genuinely outside; declaring it is what \
             makes the difference above"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Root 1's reads must not be answered out of root 0's snapshot. The
    /// snapshot `Engine` carries has no root dimension at all — it is one flat
    /// vpath tree, published for root 0 — so feeding root 1's remainder into
    /// it would answer `root1\Data\foo.esp` with root 0's backing file. That
    /// would be a *worse* failure than the pass-through this task removed:
    /// silently serving one root's bytes for another root's path.
    ///
    /// So the snapshot half of `decide` is root-0 only, and a root-≥1 read
    /// nothing local can vouch for is sealed (`Deny`) exactly the way gate 3
    /// seals an under-root path the provider graph does not know.
    #[test]
    fn a_read_under_a_second_root_is_not_answered_from_root_zeros_snapshot() {
        let base = std::env::temp_dir()
            .join(format!("vfs-engine-2root-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root0 = base.join("root0");
        let root1 = base.join("root1");
        std::fs::create_dir_all(root0.join("Data")).unwrap();
        std::fs::create_dir_all(root1.join("Data")).unwrap();

        let engine = Engine::with_roots(
            &[
                (RootId::DEFAULT, root0.to_string_lossy().into_owned()),
                (RootId(1), root1.to_string_lossy().into_owned()),
            ],
            snapshot_bytes(),
        )
        .unwrap();

        // The snapshot has exactly one entry, `data/foo.esp` -> D:\Mods\Cool.
        let via_root0 = format!(r"\??\{}", root0.join("Data").join("foo.esp").to_string_lossy());
        assert_eq!(
            engine.decide(&via_root0),
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() },
            "root 0 still resolves against the snapshot"
        );

        // The *same relative path* under root 1 must not pick that up.
        let via_root1 = format!(r"\??\{}", root1.join("Data").join("foo.esp").to_string_lossy());
        assert_eq!(
            engine.decide(&via_root1),
            Decision::Deny,
            "root 1's Data\\foo.esp must not resolve to root 0's backing file"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Copy-on-write under root 1 seeds from root 1's own real file, never
    /// from root 0's snapshot — the same cross-root confusion as
    /// `a_read_under_a_second_root_is_not_answered_from_root_zeros_snapshot`,
    /// but on the write path, where it would bake the wrong bytes into the
    /// overlay copy the game then edits.
    ///
    /// No hooks are installed here, so `cow_seed`'s real-file copy runs
    /// unmediated; inside an injected process that same copy is decided by
    /// `decide` and declines for a path under a managed root (see
    /// `cow_seed`'s doc comment). The claim this test pins is the one that
    /// holds either way: whatever the seed comes from, it is never root 0's
    /// snapshot entry for the same relative path.
    #[test]
    fn copy_on_write_under_a_second_root_seeds_from_that_root_s_real_file() {
        use crate::overlay_layer_dir;
        use vfs_redirect::FILE_OPEN_IF;
        const WRITE: u32 = 0x4000_0000;

        let base = std::env::temp_dir().join(format!("vfs-engine-2root-cow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root0 = base.join("root0");
        let root1 = base.join("root1");
        let overlay_dir = base.join("overlay");
        std::fs::create_dir_all(root0.join("Data")).unwrap();
        std::fs::create_dir_all(root1.join("Data")).unwrap();
        std::fs::create_dir_all(&overlay_dir).unwrap();
        // A real file under root 1 at the *same* relative path the snapshot
        // publishes for root 0.
        std::fs::write(root1.join("Data").join("foo.esp"), b"ROOT-1 REAL BYTES").unwrap();

        let engine = Engine::with_roots_and_overlay(
            &[
                (RootId::DEFAULT, root0.to_string_lossy().into_owned()),
                (RootId(1), root1.to_string_lossy().into_owned()),
            ],
            overlay_dir.to_str().unwrap(),
            snapshot_bytes(),
        )
        .unwrap();

        let via_root1 = format!(r"\??\{}", root1.join("Data").join("foo.esp").to_string_lossy());
        let d = engine.decide_open(&via_root1, WRITE, FILE_OPEN_IF);
        let seeded = overlay_layer_dir(&overlay_dir, RootId(1)).join("data").join("foo.esp");
        assert_eq!(d, Decision::Redirect { target_nt: to_nt(&seeded.to_string_lossy()) });
        assert_eq!(
            std::fs::read(&seeded).unwrap(),
            b"ROOT-1 REAL BYTES",
            "the overlay copy must be seeded from root 1's own file, not from \
             root 0's snapshot entry for the same relative path"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A delete under root 1 whites out root 1's copy and leaves root 0's
    /// alone. `whiteout` is the one write-path entry point with no return
    /// value to inspect, so the proof is what the *other* root reads back
    /// afterwards.
    #[test]
    fn a_delete_under_a_second_root_does_not_hide_root_zeros_file() {
        let base = std::env::temp_dir().join(format!("vfs-engine-2root-wh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root0 = base.join("root0");
        let root1 = base.join("root1");
        let overlay_dir = base.join("overlay");
        std::fs::create_dir_all(root0.join("Data")).unwrap();
        std::fs::create_dir_all(root1.join("Data")).unwrap();
        std::fs::create_dir_all(&overlay_dir).unwrap();

        let engine = Engine::with_roots_and_overlay(
            &[
                (RootId::DEFAULT, root0.to_string_lossy().into_owned()),
                (RootId(1), root1.to_string_lossy().into_owned()),
            ],
            overlay_dir.to_str().unwrap(),
            snapshot_bytes(),
        )
        .unwrap();

        let via_root1 = format!(r"\??\{}", root1.join("Data").join("foo.esp").to_string_lossy());
        assert!(engine.whiteout(&via_root1), "a delete under root 1 must be handled");
        assert!(matches!(
            engine.overlay_state(&via_root1),
            Some(OverlayState::Whiteout)
        ));

        // Root 0's identical relative path is untouched: still the snapshot's.
        let via_root0 = format!(r"\??\{}", root0.join("Data").join("foo.esp").to_string_lossy());
        assert!(
            matches!(engine.overlay_state(&via_root0), Some(OverlayState::Absent)),
            "root 1's whiteout leaked into root 0"
        );
        assert_eq!(
            engine.decide(&via_root0),
            Decision::Redirect { target_nt: r"\??\D:\Mods\Cool\foo.esp".to_string() }
        );

        let _ = std::fs::remove_dir_all(&base);
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
