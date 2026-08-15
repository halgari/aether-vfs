//! The on-disk write overlay: created/modified files land here; deletions leave
//! whiteout markers. Read resolution consults it before the snapshot. Pure `std`
//! filesystem access — no `unsafe`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use vfs_core::{fold, wildcard_match};
use vfs_redirect::{is_whiteout, whiteout_marker, DirItem, RootId};

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
/// fixed here one layer up, before `Engine` (still single-root; see the
/// `SINGLE ROOT` notes in `engine.rs`) becomes multi-root and the collision
/// would otherwise go live.
pub struct Overlay {
    root: PathBuf,
}

impl Overlay {
    pub fn new(overlay_root: &str) -> Overlay {
        Overlay { root: PathBuf::from(overlay_root) }
    }

    /// The root-scoped overlay subdirectory: distinct roots get distinct
    /// on-disk subtrees so identical relative paths under different roots
    /// never collide. See the module-level fix note above.
    fn root_dir(&self, root: RootId) -> PathBuf {
        self.root.join(format!("root-{}", root.0))
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

    /// Ensure the parent directory of `root`'s overlay file for `comps` exists.
    pub fn ensure_parent(&self, root: RootId, comps: &[String]) {
        if let Some(parent) = self.file_path(root, comps).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }

    /// Remove any whiteout marker hiding `root`'s `comps` (a path is being
    /// recreated).
    pub fn clear_whiteout(&self, root: RootId, comps: &[String]) {
        let _ = std::fs::remove_file(self.whiteout_path(root, comps));
    }

    /// Whiteout `root`'s `comps`: drop any overlay copy and lay down a marker
    /// so the path reads as deleted (hiding the snapshot backing / real file
    /// beneath).
    pub fn whiteout(&self, root: RootId, comps: &[String]) {
        self.ensure_parent(root, comps);
        let _ = std::fs::remove_file(self.file_path(root, comps));
        let _ = std::fs::write(self.whiteout_path(root, comps), b"");
    }

    /// Move `from` to `to` within `root`'s overlay subtree and whiteout the
    /// source location. The caller ensures `from` is materialized in the
    /// overlay first.
    pub fn rename(&self, root: RootId, from: &[String], to: &[String]) {
        self.ensure_parent(root, to);
        let _ = std::fs::rename(self.file_path(root, from), self.file_path(root, to));
        self.clear_whiteout(root, to);
        let _ = std::fs::write(self.whiteout_path(root, from), b"");
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

        ov.ensure_parent(RootId(0), &comps);
        std::fs::write(ov.file_path(RootId(0), &comps), b"AAAA").unwrap();

        ov.ensure_parent(RootId(1), &comps);
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
        ov.whiteout(RootId(0), &comps);
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
}
