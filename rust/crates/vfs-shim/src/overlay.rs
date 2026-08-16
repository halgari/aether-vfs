//! The on-disk write overlay: created/modified files land here; deletions leave
//! whiteout markers. Read resolution consults it before the snapshot. Pure `std`
//! filesystem access — no `unsafe`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use vfs_core::{fold, wildcard_match};
use vfs_redirect::{is_whiteout, whiteout_marker, DirItem, RootId};

/// The physical subdirectory an [`Overlay`] rooted at `overlay_root` uses for
/// `root`'s writes — [`Overlay::root_dir`] calls this too, so it is the one
/// place the naming scheme is defined.
///
/// Exposed (re-exported at the crate root) because the shim's local overlay
/// is not the only thing that reads this directory: a host-side session can
/// separately mount a read layer (e.g. a `DiskProvider`) over the same
/// physical directory so the director sees what the overlay writes, without
/// the shim and the director ever talking to each other about it — the
/// filesystem is the shared state. That caller needs the exact subtree the
/// overlay actually uses, not a re-derived or hardcoded guess at it. See
/// `vfs-director::Session::overlay_layer_dir` and its caller in
/// `vfs-directord/src/bin/skyrim-live.rs`, which mounts
/// `overlay_layer_dir(&overrides, RootId::DEFAULT)` instead of `&overrides`
/// itself for exactly this reason.
pub fn overlay_layer_dir(overlay_root: &Path, root: RootId) -> PathBuf {
    overlay_root.join(format!("root-{}", root.0))
}

/// What the overlay says about a path.
pub enum OverlayState {
    /// An overlay file or directory exists here.
    Present { path: PathBuf, is_dir: bool, size: u64 },
    /// A whiteout marker hides this path (mod-deleted at runtime).
    Whiteout,
    /// The overlay has nothing for this path; fall through to snapshot/real.
    Absent,
}

/// The overlay directory. Paths are addressed by `(RootId, folded comps)` —
/// comps are *folded* (lowercased), consistent with the snapshot and safe on
/// case-insensitive NTFS; the root is mixed into the on-disk layout (see
/// [`Overlay::root_dir`]) so two roots serving the same relative path never
/// share one overlay file.
///
/// This is the same collision the block cache had before this branch mixed
/// `RootId` into `CachingProvider::file_id_for` (see
/// `two_roots_same_path_size_and_mtime_do_not_collide` in `vfs-cache`) —
/// fixed here one layer up, deliberately one task ahead of `Engine` becoming
/// multi-root (gate 4, Task 3) — which is when the collision would otherwise
/// have gone live, since `Engine` now resolves each path's own `RootId` and
/// passes it to every call below.
///
/// **Upgrade note, no migration provided:** before this change, an overlay
/// directory had no root subdirectory at all — `<overlay_root>/data/x.ini`.
/// After it, the identical write lives at `<overlay_root>/root-0/data/x.ini`
/// (see [`overlay_layer_dir`]). Nothing here reads the old layout as a
/// fallback, and nothing migrates old content into the new one. **Empty any
/// overlay directory left over from before this change when upgrading** —
/// otherwise every copy-on-write edit it held reverts to the pre-existing
/// snapshot/real content (its bytes sit at a path this code no longer looks
/// at), and every whiteout stops applying, so files deleted at runtime
/// silently reappear. This was a deliberate call, not an oversight: the only
/// two callers that set a persistent overlay path are dev-harness binaries
/// with no shipped users (`vfs-directord/src/bin/skyrim-live.rs`,
/// `vfs-launch`), and a migrator would have to assume "everything at the old
/// top level belongs to root 0" — exactly the assumption multi-root `Engine`
/// (the very next task, now done) makes false.
pub struct Overlay {
    root: PathBuf,
}

impl Overlay {
    pub fn new(overlay_root: &str) -> Overlay {
        Overlay { root: PathBuf::from(overlay_root) }
    }

    /// The root-scoped overlay subdirectory: distinct roots get distinct
    /// on-disk subtrees so identical relative paths under different roots
    /// never collide. See the module-level fix note above and
    /// [`overlay_layer_dir`], which this delegates to.
    fn root_dir(&self, root: RootId) -> PathBuf {
        overlay_layer_dir(&self.root, root)
    }

    /// The overlay file path for `root`'s folded `comps`.
    pub fn file_path(&self, root: RootId, comps: &[String]) -> PathBuf {
        comps.iter().fold(self.root_dir(root), |a, c| a.join(c))
    }

    /// The whiteout marker path hiding `root`'s folded `comps`.
    fn whiteout_path(&self, root: RootId, comps: &[String]) -> PathBuf {
        match comps.split_last() {
            None => self.root_dir(root).join(whiteout_marker("")),
            Some((last, parents)) => {
                let dir = parents.iter().fold(self.root_dir(root), |a, c| a.join(c));
                dir.join(whiteout_marker(last))
            }
        }
    }

    fn mtime_of(md: &std::fs::Metadata) -> i64 {
        md.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// Resolve `root`'s `comps` against the overlay: overlay file wins, else
    /// whiteout hides, else absent.
    pub fn lookup(&self, root: RootId, comps: &[String]) -> OverlayState {
        if comps.is_empty() {
            return OverlayState::Absent;
        }
        let f = self.file_path(root, comps);
        if let Ok(md) = std::fs::symlink_metadata(&f) {
            return OverlayState::Present { path: f, is_dir: md.is_dir(), size: md.len() };
        }
        if self.whiteout_path(root, comps).exists() {
            return OverlayState::Whiteout;
        }
        OverlayState::Absent
    }

    /// Ensure the parent directory of `root`'s overlay file for `comps`
    /// exists.
    ///
    /// **The result is not decoration.** This used to discard it, and the
    /// discarded failure was worse than it looks: `Engine::decide_open` calls
    /// this and then answers `Decision::Redirect` at a path inside the
    /// directory that was *not* created, so the game's own open fails at the
    /// NT boundary with nothing anywhere saying why — no copy-up runs for a
    /// truncating/creating write, so not even the copy-up counters see it.
    /// Every caller now reports it (`hookstats::OverlayFail`).
    pub fn ensure_parent(&self, root: RootId, comps: &[String]) -> std::io::Result<()> {
        match self.file_path(root, comps).parent() {
            Some(parent) => std::fs::create_dir_all(parent),
            None => Ok(()),
        }
    }

    /// Remove any whiteout marker hiding `root`'s `comps` (a path is being
    /// recreated). No marker is success — there was nothing to clear.
    pub fn clear_whiteout(&self, root: RootId, comps: &[String]) -> std::io::Result<()> {
        match std::fs::remove_file(self.whiteout_path(root, comps)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    /// Whiteout `root`'s `comps`: drop any overlay copy and lay down a marker
    /// so the path reads as deleted (hiding the snapshot backing / real file
    /// beneath).
    ///
    /// Removing the overlay copy stays best-effort — there usually is none,
    /// and its absence is the normal case, not a failure. Failing to write
    /// the marker is not: the path stays *visible* afterward, which is a
    /// deleted file that comes back.
    pub fn whiteout(&self, root: RootId, comps: &[String]) -> std::io::Result<()> {
        self.ensure_parent(root, comps)?;
        let _ = std::fs::remove_file(self.file_path(root, comps));
        std::fs::write(self.whiteout_path(root, comps), b"")
    }

    /// Move `from` to `to` within `root`'s overlay subtree and whiteout the
    /// source location. The caller ensures `from` is materialized in the
    /// overlay first.
    ///
    /// A missing source is tolerated: copy-up is best-effort by design (see
    /// `Engine::copy_up`), so a director that declined to hand over the
    /// content leaves nothing at `from`, and the rename of an absent file is
    /// the expected shape of that — already counted as a copy-up failure at
    /// its own site. Anything else is reported.
    pub fn rename(&self, root: RootId, from: &[String], to: &[String]) -> std::io::Result<()> {
        self.ensure_parent(root, to)?;
        match std::fs::rename(self.file_path(root, from), self.file_path(root, to)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        self.clear_whiteout(root, to)?;
        std::fs::write(self.whiteout_path(root, from), b"")
    }

    /// Whether an overlay file exists for `root`'s `comps`.
    pub fn has_file(&self, root: RootId, comps: &[String]) -> bool {
        self.file_path(root, comps).exists()
    }

    /// Overlay `root`'s directory overlay entries onto a snapshot+real
    /// `merged` listing: whiteout markers remove names, overlay files
    /// add/override (wildcard-filtered), result stays folded-ordered.
    pub fn apply_to_listing(
        &self,
        root: RootId,
        dir_comps: &[String],
        merged: Vec<DirItem>,
        wildcard: Option<&str>,
    ) -> Vec<DirItem> {
        let dir = self.file_path(root, dir_comps);
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => return merged, // no overlay dir here -> nothing to apply
        };
        let mut map: BTreeMap<String, DirItem> = BTreeMap::new();
        for it in merged {
            map.insert(fold(&it.name), it);
        }
        let mut adds: Vec<DirItem> = Vec::new();
        for e in read.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(base) = is_whiteout(&name) {
                map.remove(&fold(base));
                // …and the marker's own name, which `merged` may carry. Since
                // gate 4 Task 6 a host mounts this very directory into the
                // director's graph as a write layer (see `overlay_layer_dir`),
                // and the director uses a different marker convention
                // (`vfs_compose::OverlayProvider`'s `.wh.<name>` prefix), so
                // it has no reason to hide ours: a listing that includes them
                // would otherwise show the game a real file called
                // `<name>.__vfs_wh__`.
                //
                // **Neither of these two lines can fire in production today,
                // and the Task 6 review that added the second one did not say
                // so.** Both act on `merged`, and `merged` is empty on every
                // production path: `Engine::overlay_listing` is the only
                // caller, `hook.rs`'s `ContainedNoDirector` arm is *its* only
                // caller, and that arm passes `&[]` — its whole purpose is an
                // overlay-only listing with no base to layer onto. That arm is
                // additionally dead by measurement (see its own comment for
                // why it is kept: it is fail-closed insurance against the two
                // root predicates drifting apart, which they have before).
                //
                // Kept rather than deleted, because this is a general overlay
                // function and this is the right behaviour for any merged
                // listing it is ever handed — and because deleting "the
                // mitigation" alone is incoherent: the pre-Task-6 line above
                // is dead for the identical reason, so removal would have to
                // take the whole merged-listing path and the fail-closed hook
                // arm with it.
                //
                // **Where the phantom marker actually surfaces is elsewhere,
                // and is not fixed here.** The live enumeration path is
                // `hook.rs`'s director branch (`client.readdir`), which never
                // calls this function. A shim whiteout therefore does show
                // the game a `<file>.__vfs_wh__` entry and does not hide the
                // file it names. This used to name the DRM-exception route as
                // how such a whiteout gets written; gate 5 Task 4 deleted that
                // route, and the remaining one is the `allow_disk_fallthrough`
                // opt-out. Task 7 owns the fix and should re-derive the
                // reachability rather than inherit this claim.
                map.remove(&fold(&name));
                continue;
            }
            let md = match e.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            adds.push(DirItem { name, is_dir: md.is_dir(), size: md.len(), mtime: Self::mtime_of(&md) });
        }
        for a in adds {
            if wildcard.map(|w| wildcard_match(w, &a.name)).unwrap_or(true) {
                map.insert(fold(&a.name), a);
            }
        }
        map.into_values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identical collision `CachingProvider` had before `RootId` was
    /// mixed into `file_id_for` (see
    /// `two_roots_same_path_size_and_mtime_do_not_collide` in
    /// `vfs-cache/src/provider.rs`), one layer up: two roots serving the same
    /// relative path must not share one overlay file, and a whiteout written
    /// under one root must not hide the other root's file.
    #[test]
    fn two_roots_same_path_do_not_collide() {
        let dir = std::env::temp_dir()
            .join(format!("vfs-overlay-tworoots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ov = Overlay::new(dir.to_str().unwrap());
        let comps = vec!["data".to_string(), "foo.esp".to_string()];

        ov.ensure_parent(RootId(0), &comps).unwrap();
        std::fs::write(ov.file_path(RootId(0), &comps), b"AAAA").unwrap();

        ov.ensure_parent(RootId(1), &comps).unwrap();
        std::fs::write(ov.file_path(RootId(1), &comps), b"BBBB").unwrap();

        match ov.lookup(RootId(0), &comps) {
            OverlayState::Present { path, .. } => {
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    b"AAAA",
                    "root 0 should read its own bytes"
                );
            }
            _ => panic!("root 0's overlay file went missing"),
        }
        match ov.lookup(RootId(1), &comps) {
            OverlayState::Present { path, .. } => {
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    b"BBBB",
                    "root 1 got root 0's overlay bytes back -- comps collided across roots"
                );
            }
            _ => panic!("root 1's overlay file went missing"),
        }

        // A whiteout under root 0 must not hide root 1's file at the same
        // relative path.
        ov.whiteout(RootId(0), &comps).unwrap();
        assert!(
            matches!(ov.lookup(RootId(0), &comps), OverlayState::Whiteout),
            "root 0 should read as whited-out"
        );
        match ov.lookup(RootId(1), &comps) {
            OverlayState::Present { path, .. } => {
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    b"BBBB",
                    "root 1's file must survive root 0's whiteout"
                );
            }
            OverlayState::Whiteout => panic!("root 0's whiteout leaked into root 1's lookup"),
            OverlayState::Absent => panic!("root 1's overlay file went missing"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Gate 4, Task 6 review, minor 1. The shim and the director share one
    /// physical overlay directory but spell whiteouts differently — the shim
    /// appends `.__vfs_wh__`, `vfs_compose::OverlayProvider` prefixes `.wh.` —
    /// and neither hides the other's markers.
    ///
    /// **This test constructs a `merged` listing that no production caller
    /// produces**, and the version of this comment written with the mitigation
    /// said the opposite: that the director's listing of the shared directory
    /// "therefore arrives at `apply_to_listing`". It does not, twice over.
    /// `Engine::overlay_listing` is `apply_to_listing`'s only caller,
    /// `hook.rs`'s `ContainedNoDirector` arm is its only caller, and that arm
    /// passes an empty base — so `merged` is always `[]` in production, even
    /// before accounting for that arm being dead by measurement. The live
    /// enumeration path is the director branch, which never reaches here.
    ///
    /// So this is a **unit test of `apply_to_listing`'s contract**, not
    /// evidence of a live behaviour, and it is worth keeping only as that:
    /// the fail-closed hook arm is deliberately retained against predicate
    /// drift, and if it revives with a non-empty base this is the assertion
    /// that says what the function must then do. It is *not* evidence that a
    /// shim whiteout is hidden from the game today — on the live director
    /// path it is not, which is recorded at the call site and belongs to
    /// gate 5.
    #[test]
    fn a_whiteout_marker_is_dropped_from_a_merged_listing_that_carries_it() {
        let dir = std::env::temp_dir()
            .join(format!("vfs-overlay-marker-listing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ov = Overlay::new(dir.to_str().unwrap());
        let comps = vec!["data".to_string(), "gone.esp".to_string()];
        ov.whiteout(RootId(0), &comps).unwrap();

        // What the director hands back for `data/` once the same directory is
        // mounted as its write layer: the deleted file (still in the read
        // layers) *and* our marker, which the director has no reason to hide.
        let marker = vfs_redirect::whiteout_marker("gone.esp");
        let merged = vec![
            DirItem { name: "gone.esp".into(), is_dir: false, size: 10, mtime: 0 },
            DirItem { name: marker.clone(), is_dir: false, size: 0, mtime: 0 },
            DirItem { name: "kept.esp".into(), is_dir: false, size: 20, mtime: 0 },
        ];

        let out = ov.apply_to_listing(RootId(0), &["data".to_string()], merged, None);
        let names: Vec<&str> = out.iter().map(|i| i.name.as_str()).collect();
        assert!(
            !names.iter().any(|n| *n == marker),
            "the tombstone surfaced to the caller as a real file: {names:?}"
        );
        assert!(
            !names.contains(&"gone.esp"),
            "the whiteout must still hide the file it names: {names:?}"
        );
        assert!(
            names.contains(&"kept.esp"),
            "unrelated entries must survive: {names:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `two_roots_same_path_do_not_collide`'s whiteout check above cannot
    /// fail on its own: `lookup` checks the overlay file before the whiteout
    /// marker (see `lookup`'s body), and that test's root 1 always has its
    /// own file at the shared path, so `Present` shadows the marker check
    /// entirely regardless of whether `whiteout_path` is root-scoped. This
    /// test removes that shadow: root 1 has no overlay file at all for this
    /// path, so its `lookup` must fall through to the whiteout-marker check —
    /// exactly the branch that would expose a root-blind `whiteout_path`
    /// (root 0's marker landing somewhere root 1's lookup also checks).
    #[test]
    fn whiteout_under_one_root_is_absent_not_whiteout_under_another() {
        let dir = std::env::temp_dir()
            .join(format!("vfs-overlay-wh-noleak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ov = Overlay::new(dir.to_str().unwrap());
        let comps = vec!["data".to_string(), "solo.esp".to_string()];

        // Root 0 only: root 1 never had anything at this path.
        ov.ensure_parent(RootId(0), &comps).unwrap();
        std::fs::write(ov.file_path(RootId(0), &comps), b"ROOT0-ONLY").unwrap();
        ov.whiteout(RootId(0), &comps).unwrap();

        assert!(
            matches!(ov.lookup(RootId(0), &comps), OverlayState::Whiteout),
            "root 0 should read as whited-out"
        );
        assert!(
            matches!(ov.lookup(RootId(1), &comps), OverlayState::Absent),
            "root 0's whiteout marker leaked into root 1's lookup for a path root 1 never wrote"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
