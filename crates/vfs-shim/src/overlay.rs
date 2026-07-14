//! The on-disk write overlay: created/modified files land here; deletions leave
//! whiteout markers. Read resolution consults it before the snapshot. Pure `std`
//! filesystem access — no `unsafe`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use vfs_core::{fold, wildcard_match};
use vfs_redirect::{is_whiteout, whiteout_marker, DirItem};

/// What the overlay says about a path.
pub enum OverlayState {
    /// An overlay file or directory exists here.
    Present { path: PathBuf, is_dir: bool, size: u64, mtime: i64 },
    /// A whiteout marker hides this path (mod-deleted at runtime).
    Whiteout,
    /// The overlay has nothing for this path; fall through to snapshot/real.
    Absent,
}

/// The overlay directory. Paths are addressed by *folded* (lowercased)
/// components — consistent with the snapshot and safe on case-insensitive NTFS.
pub struct Overlay {
    root: PathBuf,
}

impl Overlay {
    pub fn new(overlay_root: &str) -> Overlay {
        Overlay { root: PathBuf::from(overlay_root) }
    }

    /// The overlay file path for folded `comps`.
    pub fn file_path(&self, comps: &[String]) -> PathBuf {
        comps.iter().fold(self.root.clone(), |a, c| a.join(c))
    }

    /// The whiteout marker path hiding folded `comps`.
    fn whiteout_path(&self, comps: &[String]) -> PathBuf {
        match comps.split_last() {
            None => self.root.join(whiteout_marker("")),
            Some((last, parents)) => {
                let dir = parents.iter().fold(self.root.clone(), |a, c| a.join(c));
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

    /// Resolve `comps` against the overlay: overlay file wins, else whiteout
    /// hides, else absent.
    pub fn lookup(&self, comps: &[String]) -> OverlayState {
        if comps.is_empty() {
            return OverlayState::Absent;
        }
        let f = self.file_path(comps);
        if let Ok(md) = std::fs::symlink_metadata(&f) {
            return OverlayState::Present {
                path: f,
                is_dir: md.is_dir(),
                size: md.len(),
                mtime: Self::mtime_of(&md),
            };
        }
        if self.whiteout_path(comps).exists() {
            return OverlayState::Whiteout;
        }
        OverlayState::Absent
    }

    /// Overlay a directory's overlay entries onto a snapshot+real `merged`
    /// listing: whiteout markers remove names, overlay files add/override
    /// (wildcard-filtered), result stays folded-ordered.
    pub fn apply_to_listing(
        &self,
        dir_comps: &[String],
        merged: Vec<DirItem>,
        wildcard: Option<&str>,
    ) -> Vec<DirItem> {
        let dir = self.file_path(dir_comps);
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
