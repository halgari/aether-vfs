//! The whiteout index: which names each upper directory hides with a `.wh.`
//! marker, and the operations that read and keep it.

use std::collections::{HashMap, HashSet};

use vfs_core::fold;
use vfs_provider::{
    map_io_err, not_a_dir, not_found, whiteout_name, VPath, OPEN_CREATE, OPEN_TRUNC, OPEN_WRITE,
    WHITEOUT_PREFIX,
};

use super::OverlayProvider;

/// [`OverlayProvider::whiteouts`].
#[derive(Default)]
pub(super) struct WhiteoutIndex {
    /// Bumped whenever a marker is written or cleared, or a scanned
    /// directory is dropped for a rescan.
    generation: u64,
    /// Root → folded directory → the folded names its markers hide.
    dirs: HashMap<u32, HashMap<String, HashSet<String>>>,
}

impl OverlayProvider {
    /// `.wh.<name>` sibling of `path`, in the same directory.
    pub(super) fn whiteout_path(&self, path: &str) -> String {
        match path.rsplit_once('/') {
            Some((parent, name)) => format!("{parent}/{}", whiteout_name(name)),
            None => whiteout_name(path),
        }
    }

    /// `(parent directory, name)` for `rel`; the parent of a top-level name
    /// is the empty root path.
    pub(super) fn split_parent(rel: &str) -> (&str, &str) {
        match rel.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", rel),
        }
    }

    /// Every name the upper's `dir` hides with a `.wh.` marker, folded. A
    /// directory the upper does not have (or has as a file) hides nothing.
    pub(super) fn scan_whiteouts(&self, dir: VPath) -> Result<HashSet<String>, i32> {
        let mut hidden = HashSet::new();
        match self.upper.readdir(dir) {
            Ok(entries) => {
                for e in entries {
                    if let Some(base) = e.name.strip_prefix(WHITEOUT_PREFIX) {
                        hidden.insert(fold(base));
                    }
                }
            }
            Err(e) if e == not_found() || e == not_a_dir() => {}
            Err(e) => return Err(e),
        }
        Ok(hidden)
    }

    /// Answered from [`OverlayProvider::whiteouts`] — see that field for why
    /// this is an index lookup rather than the `upper.getattr` per ancestor
    /// it used to be.
    ///
    /// Folded on both sides, matching `readdir`'s own whiteout matching. On a
    /// case-insensitive upper that is what the filesystem was doing anyway;
    /// on a case-sensitive one it is the behaviour the rest of this file
    /// already assumes.
    pub(super) fn is_whiteout(&self, p: VPath) -> Result<bool, i32> {
        self.whiteout_walk(p, true, false)
    }

    /// The whiteout question for `p` itself (`own`), for its ancestor
    /// directories (`ancestors`), or for both: true at the first one hidden,
    /// looking at `p` first and then outwards, one directory at a time.
    ///
    /// This runs on every `getattr`, `open` and `readdir`, nearly always to
    /// answer "no", so it is written to cost one fold, one lock and no
    /// allocation once the directories involved have been scanned:
    ///
    /// - **One fold.** `fold` maps a path character by character, `/` only
    ///   to `/` and nothing else to it, so the folded parent of a path is
    ///   the parent of the folded path. The walk splits `p.rel` and its fold
    ///   at the same separators instead of folding each ancestor's parent
    ///   and name again.
    /// - **One lock, shared**, held across the walk, so concurrent lookups
    ///   do not queue behind each other. It is released only to scan a
    ///   directory seen for the first time (an `upper.readdir`) and store
    ///   the result, exactly as before; the walk then starts again from `p`.
    pub(super) fn whiteout_walk(&self, p: VPath, own: bool, ancestors: bool) -> Result<bool, i32> {
        let folded = fold(p.rel);
        loop {
            // The directory the walk reached that has not been scanned yet,
            // in the caller's spelling and folded, and the index's generation
            // when the walk found it so.
            let (raw_dir, fol_dir, generation) = {
                let index = self.whiteouts.read().map_err(|_| map_io_err())?;
                let dirs = index.dirs.get(&p.root.0);
                let (mut raw, mut fol) = (p.rel, folded.as_str());
                let mut check = own;
                loop {
                    let (raw_parent, _) = Self::split_parent(raw);
                    let (fol_parent, fol_name) = Self::split_parent(fol);
                    if check {
                        match dirs.and_then(|d| d.get(fol_parent)) {
                            Some(hidden) if hidden.contains(fol_name) => return Ok(true),
                            Some(_) => {}
                            None => break (raw_parent, fol_parent, index.generation),
                        }
                    }
                    if !ancestors || !raw.contains('/') {
                        return Ok(false);
                    }
                    (raw, fol) = (raw_parent, fol_parent);
                    check = true;
                }
            };
            // First look inside this directory. One readdir answers it for
            // this path, all its siblings, and every later ancestor walk
            // through it. Not under the lock: it is a call into the upper.
            // The walk then starts over, and finds this directory scanned.
            let hidden = self.scan_whiteouts(VPath::new(p.root, raw_dir))?;
            let mut index = self.whiteouts.write().map_err(|_| map_io_err())?;
            // A marker written or cleared since the walk released the lock
            // may be missing from this scan, and `note_whiteout` could not
            // record it in a directory that was not scanned yet. Storing the
            // scan would then hide the change until something dropped the
            // directory. Throw it away; the walk starts over and scans again.
            if index.generation == generation {
                index
                    .dirs
                    .entry(p.root.0)
                    .or_default()
                    .insert(fol_dir.to_owned(), hidden);
            }
        }
    }

    /// Record that `p`'s marker now exists (`hidden`) or no longer does, in
    /// whichever directory entry the index has already scanned. A directory
    /// not yet scanned has no entry to update: its first scan will see the
    /// marker's real state on disk — and a scan already under way, which may
    /// have read the directory before this change, is discarded by the
    /// generation bump.
    pub(super) fn note_whiteout(&self, p: VPath, hidden: bool) {
        let (parent, name) = Self::split_parent(p.rel);
        let Ok(mut g) = self.whiteouts.write() else {
            return;
        };
        g.generation += 1;
        let scanned = g.dirs.get_mut(&p.root.0);
        if let Some(set) = scanned.and_then(|d| d.get_mut(&fold(parent))) {
            if hidden {
                set.insert(fold(name));
            } else {
                set.remove(&fold(name));
            }
        }
    }

    /// Drop the scanned index for `p`'s directory when `p` itself names a
    /// `.wh.` marker.
    ///
    /// The module docs reserve `.wh.*` inside the upper, but nothing stops a
    /// caller *creating* such a name through this provider (a write, a mkdir,
    /// a rename destination). Before the index that was self-correcting —
    /// the next `is_whiteout` read the filesystem. Now the directory has to
    /// be rescanned, or `readdir` (which scans the upper live) and
    /// `getattr`/`open` (which read the index) would disagree about whether
    /// the sibling it names is hidden.
    pub(super) fn invalidate_if_marker(&self, p: VPath) {
        let (parent, name) = Self::split_parent(p.rel);
        if !name.starts_with(WHITEOUT_PREFIX) {
            return;
        }
        if let Ok(mut g) = self.whiteouts.write() {
            // Also discards a scan of this directory already under way.
            g.generation += 1;
            if let Some(dirs) = g.dirs.get_mut(&p.root.0) {
                dirs.remove(&fold(parent));
            }
        }
    }

    /// True if any ancestor directory of `p` (not `p` itself) has been
    /// whited out. Deliberately kept separate from [`Self::is_whiteout`]:
    /// `open_for_write`'s `OPEN_CREATE` handling must tell "this exact path
    /// was removed" (safe to un-hide by creating it again) from "an ancestor
    /// directory was opaquely removed" (not safe to paper over — see the
    /// comment there).
    pub(super) fn ancestor_whited_out(&self, p: VPath) -> Result<bool, i32> {
        self.whiteout_walk(p, false, true)
    }

    /// True if `p` itself, or any ancestor directory, has been whited out —
    /// a whiteout on a base directory hides its whole subtree.
    pub(super) fn hidden_by_whiteout(&self, p: VPath) -> Result<bool, i32> {
        self.whiteout_walk(p, true, true)
    }

    pub(super) fn clear_whiteout(&self, p: VPath) -> Result<(), i32> {
        let wh = self.whiteout_path(p.rel);
        match self.upper.remove(VPath::new(p.root, &wh)) {
            Ok(()) => {
                self.note_whiteout(p, false);
                Ok(())
            }
            Err(e) if e == not_found() => {
                // Nothing was hiding it; the index must agree either way.
                self.note_whiteout(p, false);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    pub(super) fn write_whiteout(&self, p: VPath) -> Result<(), i32> {
        let wh = self.whiteout_path(p.rel);
        let (h, _, _) = self.upper.open(
            VPath::new(p.root, &wh),
            OPEN_WRITE | OPEN_CREATE | OPEN_TRUNC,
        )?;
        self.upper.close(h)?;
        self.note_whiteout(p, true);
        Ok(())
    }
}
