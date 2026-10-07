//! Upper-over-base with `.wh.*` whiteouts and whole-file copy-up.
//!
//! Base is read through the `Provider` interface and never mutated. Upper is
//! itself a `Provider` and must declare `Access::ReadWrite` — validated once
//! at construction, not at first write, so a misconfigured stack fails fast
//! rather than surprising the first writer. Writing to a base-only path
//! copies the whole file into upper before the write lands: the domain here
//! is INIs, saves, and logs, not the multi-gigabyte read-only assets, so
//! whole-file copy-up beats a lazy, block-tracked one on both simplicity and
//! correctness. Removing a base-visible path — file or directory — writes a
//! `.wh.<name>` marker into upper instead of touching base.
//!
//! Copy-up stages into a `.cu.<n>.<name>` temp file in the same directory and
//! renames it over the destination on success, so the destination never
//! exists in a partially-copied state: a reader either sees the whole file or
//! none of it, and a failed copy leaves nothing behind for a later check to
//! mistake for "already copied".
//!
//! Both prefixes are reserved names within upper: a real file genuinely named
//! `.wh.foo` or `.cu.1.foo` is shadowed (treated as a marker, not served as
//! content). This is the standard overlayfs tradeoff, made explicit here
//! rather than left to be discovered.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use vfs_core::fold;
use vfs_provider::{
    bad_fh, bad_request, is_dir, map_io_err, not_a_dir, not_found, not_supported, Access,
    Capabilities, DirEntry, Handle, Provider, SetAttr, Stat, VPath, KIND_DIR, KIND_FILE,
    OPEN_CREATE, OPEN_READ, OPEN_TRUNC, OPEN_WRITE,
};

#[derive(Clone, Copy)]
enum Layer {
    Upper,
    Base,
}

/// Upper-over-base with `.wh.*` whiteouts and copy-up (see module docs).
pub struct OverlayProvider {
    base: Arc<dyn Provider>,
    upper: Arc<dyn Provider>,
    next: AtomicU64,
    opens: Mutex<HashMap<u64, (Layer, Handle)>>,
    /// Paths currently being copied up, so two concurrent writers to the same
    /// base-only path copy exactly once instead of racing.
    copying: Mutex<HashSet<String>>,
    /// Which names each upper directory hides with a `.wh.` marker, keyed by
    /// root and then by folded parent path, and holding the *folded base
    /// names* the markers refer to. A present entry means that directory has
    /// been scanned; a missing one means it has not. (Two levels rather than
    /// one `(root, path)` key so a lookup borrows the path instead of
    /// allocating a key for every ancestor: see [`Self::whiteout_walk`].)
    ///
    /// **This exists for the read path, not the write path.** Every
    /// `getattr`, `open` and `readdir` has to answer "is this path, or any
    /// ancestor directory of it, whited out?", and the direct implementation
    /// is one `upper.getattr` per ancestor — `depth + 1` filesystem
    /// `metadata` calls on *every* read of *every* file. Under a game load
    /// that is six figures of syscalls doing nothing, in a harness whose
    /// other job is measuring load time.
    ///
    /// One `upper.readdir` of a directory answers the question for that
    /// directory's whole contents at once, so the index costs one readdir per
    /// distinct directory ever touched (first touch only; a directory absent
    /// from the upper — the common case, since the upper is a write layer —
    /// costs a single failed call) and zero filesystem calls per operation
    /// afterwards.
    ///
    /// It is safe to cache because **this provider is the only writer of
    /// `.wh.` markers in its own upper**: they are created only by
    /// [`OverlayProvider::write_whiteout`] and removed only by
    /// [`OverlayProvider::clear_whiteout`], both of which update the index in
    /// the same step. A process outside this provider mutating the upper's
    /// markers underneath us is already outside the contract — the upper is
    /// the overlay's private store.
    ///
    /// The index also carries a generation, bumped by every marker change
    /// this provider makes: a directory's first scan runs outside the lock,
    /// and its result is stored only if no marker changed meanwhile (see
    /// [`Self::whiteout_walk`]).
    whiteouts: RwLock<WhiteoutIndex>,
}

/// [`OverlayProvider::whiteouts`].
#[derive(Default)]
struct WhiteoutIndex {
    /// Bumped whenever a marker is written or cleared, or a scanned
    /// directory is dropped for a rescan.
    generation: u64,
    /// Root → folded directory → the folded names its markers hide.
    dirs: HashMap<u32, HashMap<String, HashSet<String>>>,
}

/// Removes `path` from the in-flight set on drop, including on early return —
/// so a failed copy still releases the slot for the next attempt.
struct CopyGuard<'a> {
    copying: &'a Mutex<HashSet<String>>,
    path: &'a str,
}

impl Drop for CopyGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut g) = self.copying.lock() {
            g.remove(self.path);
        }
    }
}

impl OverlayProvider {
    /// `upper` may be a bare `Provider` value or one already behind an `Arc`
    /// — either way it is normalized to `Arc<dyn Provider>`. Fails if `upper`
    /// does not declare `Access::ReadWrite`: that must be caught here, not at
    /// first write.
    pub fn new<U>(base: Arc<dyn Provider>, upper: U) -> Result<Self, &'static str>
    where
        U: Provider + 'static,
    {
        Self::from_arcs(base, Arc::new(upper))
    }

    /// [`OverlayProvider::new`] for an upper the caller already holds behind
    /// an `Arc` and needs to keep a handle to — the shape a host builds when
    /// the same provider object is also read for diagnostics (see
    /// `skyrim-live`'s root-1 `CountingProvider`). `new`'s type parameter
    /// cannot express that: there is no `impl Provider for Arc<dyn Provider>`
    /// in this workspace, so `new(base, arc)` would need `Arc<dyn Provider>`
    /// to itself be a `Provider` and does not compile.
    pub fn from_arcs(
        base: Arc<dyn Provider>,
        upper: Arc<dyn Provider>,
    ) -> Result<Self, &'static str> {
        if upper.capabilities().access != Access::ReadWrite {
            return Err("OverlayProvider: upper must declare Access::ReadWrite");
        }
        Ok(Self {
            base,
            upper,
            next: AtomicU64::new(1),
            opens: Mutex::new(HashMap::new()),
            copying: Mutex::new(HashSet::new()),
            whiteouts: RwLock::new(WhiteoutIndex::default()),
        })
    }

    fn track(&self, layer: Layer, inner: Handle) -> Result<Handle, i32> {
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .insert(h, (layer, inner));
        Ok(h)
    }

    fn lookup(&self, h: Handle) -> Result<(Layer, Handle), i32> {
        self.opens
            .lock()
            .map_err(|_| map_io_err())?
            .get(&h)
            .copied()
            .ok_or_else(bad_fh)
    }

    /// `.wh.<name>` sibling of `path`, in the same directory.
    fn whiteout_path(&self, path: &str) -> String {
        match path.rsplit_once('/') {
            Some((parent, name)) => format!("{parent}/.wh.{name}"),
            None => format!(".wh.{path}"),
        }
    }

    /// `.cu.<n>.<name>` sibling of `path`, in the same directory as the
    /// eventual destination — so the final rename stays within one parent
    /// (and, for a disk-backed upper, one volume) and is atomic.
    fn temp_copy_path(&self, path: &str, n: u64) -> String {
        match path.rsplit_once('/') {
            Some((parent, name)) => format!("{parent}/.cu.{n}.{name}"),
            None => format!(".cu.{n}.{path}"),
        }
    }

    /// `(parent directory, name)` for `rel`; the parent of a top-level name
    /// is the empty root path.
    fn split_parent(rel: &str) -> (&str, &str) {
        match rel.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", rel),
        }
    }

    /// Every name the upper's `dir` hides with a `.wh.` marker, folded. A
    /// directory the upper does not have (or has as a file) hides nothing.
    fn scan_whiteouts(&self, dir: VPath) -> Result<HashSet<String>, i32> {
        let mut hidden = HashSet::new();
        match self.upper.readdir(dir) {
            Ok(entries) => {
                for e in entries {
                    if let Some(base) = e.name.strip_prefix(".wh.") {
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
    fn is_whiteout(&self, p: VPath) -> Result<bool, i32> {
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
    fn whiteout_walk(&self, p: VPath, own: bool, ancestors: bool) -> Result<bool, i32> {
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
    fn note_whiteout(&self, p: VPath, hidden: bool) {
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
    fn invalidate_if_marker(&self, p: VPath) {
        let (parent, name) = Self::split_parent(p.rel);
        if !name.starts_with(".wh.") {
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
    fn ancestor_whited_out(&self, p: VPath) -> Result<bool, i32> {
        self.whiteout_walk(p, false, true)
    }

    /// True if `p` itself, or any ancestor directory, has been whited out —
    /// a whiteout on a base directory hides its whole subtree.
    fn hidden_by_whiteout(&self, p: VPath) -> Result<bool, i32> {
        self.whiteout_walk(p, true, true)
    }

    fn clear_whiteout(&self, p: VPath) -> Result<(), i32> {
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

    fn write_whiteout(&self, p: VPath) -> Result<(), i32> {
        let wh = self.whiteout_path(p.rel);
        let (h, _, _) = self.upper.open(
            VPath::new(p.root, &wh),
            OPEN_WRITE | OPEN_CREATE | OPEN_TRUNC,
        )?;
        self.upper.close(h)?;
        self.note_whiteout(p, true);
        Ok(())
    }

    /// Copy the whole base file at `p` into upper if it is not already there.
    /// A no-op if `p` is absent from base too, or is a directory (directories
    /// are represented implicitly, never copied). Guarded by `copying` so two
    /// concurrent callers for the same path copy exactly once: whoever loses
    /// the race waits for the winner's slot to clear, then re-checks upper
    /// before ever touching base.
    fn copy_up_if_needed(&self, p: VPath) -> Result<(), i32> {
        if self.upper.getattr(p)?.is_some() {
            return Ok(());
        }
        let Some(stat) = self.base.getattr(p)? else {
            return Ok(());
        };
        if stat.kind != KIND_FILE {
            return Ok(());
        }

        // Folded: two callers that spell one file differently must wait for
        // each other. Keyed by the spelling, each would copy the file up on
        // its own, and the second copy's rename would replace the file the
        // first caller had already opened for writing.
        let path = fold(p.rel);
        loop {
            let mut inflight = self.copying.lock().map_err(|_| map_io_err())?;
            if inflight.insert(path.clone()) {
                break;
            }
            drop(inflight);
            std::thread::yield_now();
        }
        let _guard = CopyGuard {
            copying: &self.copying,
            path: &path,
        };

        // Re-check: another thread may have finished the copy between our
        // first getattr above and winning the slot just now.
        if self.upper.getattr(p)?.is_some() {
            return Ok(());
        }
        self.copy_file_up(p)
    }

    /// Copies base's `p` into a `.cu.` temp file in upper and renames it over
    /// `p` only on complete success. The destination is never opened,
    /// touched, or truncated directly: if the read, a write, flush, close,
    /// or the final rename fails, the temp file is removed and the original
    /// error is propagated (not the cleanup's) — so a partial copy can never
    /// be mistaken for a complete one by a later `getattr`/copy-up check,
    /// and a concurrent reader can never observe a half-written destination.
    fn copy_file_up(&self, p: VPath) -> Result<(), i32> {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let tmp_rel = self.temp_copy_path(p.rel, n);
        let tmp = VPath::new(p.root, &tmp_rel);

        let (bh, size, _) = self.base.open(p, OPEN_READ)?;
        let copied = self.copy_bytes(bh, size, tmp);
        let _ = self.base.close(bh);

        let result = copied.and_then(|_| self.upper.rename(tmp, p));
        if result.is_err() {
            let _ = self.upper.remove(tmp);
        }
        result
    }

    fn copy_bytes(&self, bh: Handle, size: u64, dest: VPath) -> Result<(), i32> {
        let (uh, _, _) = self
            .upper
            .open(dest, OPEN_WRITE | OPEN_CREATE | OPEN_TRUNC)?;
        let copied = self.copy_loop(bh, uh, size).and_then(|_| self.upper.flush(uh));
        let closed = self.upper.close(uh);
        // Prefer the copy/flush error over the close error: it happened
        // first and is almost always the more useful one to report, but
        // either way *an* error here must never be swallowed.
        copied.and(closed)
    }

    fn copy_loop(&self, bh: Handle, uh: Handle, size: u64) -> Result<(), i32> {
        let mut buf = [0u8; 65536];
        let mut off = 0u64;
        while off < size {
            let n = self.base.read_at(bh, off, &mut buf)?;
            if n == 0 {
                break;
            }
            self.upper.write_at(uh, off, &buf[..n])?;
            off += n as u64;
        }
        Ok(())
    }

    fn open_for_write(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        if self.upper.getattr(p)?.is_none() {
            // An ancestor directory being opaquely removed is deliberately
            // NOT something OPEN_CREATE can paper over. Clearing the
            // ancestor's whiteout here would silently resurrect every other
            // base entry under it that the caller never asked to restore;
            // creating the file anyway while leaving the ancestor whiteout
            // in place would leave it permanently invisible to
            // `hidden_by_whiteout`'s ancestor walk while still showing up
            // through `readdir`'s upper merge — an inconsistent state with
            // no good reading. Refusing is the only option with no
            // surprising side effect; the way back is explicit: `mkdir` the
            // ancestor, which clears exactly its own whiteout.
            if self.ancestor_whited_out(p)? {
                return Err(not_found());
            }
            if self.is_whiteout(p)? {
                if flags & OPEN_CREATE == 0 {
                    return Err(not_found());
                }
                // OPEN_CREATE explicitly asks to (re)create over a whiteout
                // on this exact path; clear it so the new file is genuinely
                // visible afterward.
                self.clear_whiteout(p)?;
            } else {
                // The base serves a **directory** at this path. Falling
                // through to `upper.open(…, OPEN_CREATE)` below would create
                // a *file* in the upper named after it — which then shadows
                // the directory for every later lookup, and makes the whole
                // subtree unlistable. That is reachable from an ordinary
                // Windows call: `CreateFileW(dir, GENERIC_WRITE, OPEN_ALWAYS,
                // FILE_FLAG_BACKUP_SEMANTICS)` sets no `FILE_DIRECTORY_FILE`,
                // so nothing upstream recognises it as a directory open, and
                // `FILE_OPEN_IF` arrives here carrying `OPEN_CREATE`.
                //
                // `copy_up_if_needed` already declines to copy a directory,
                // but declining quietly is what let the create through.
                // Refuse instead, with the status that says why — the shim
                // turns it back into the directory open the caller wanted
                // (`hook::dir_open_downgrades`), and a caller that really did
                // mean "create a file here" gets NT's own answer for a file
                // create over a directory.
                if matches!(self.base.getattr(p)?, Some(st) if st.kind == KIND_DIR) {
                    return Err(is_dir());
                }
                self.copy_up_if_needed(p)?;
            }
        }
        let (uh, size, is_dir) = self.upper.open(p, flags)?;
        self.invalidate_if_marker(p);
        let h = self.track(Layer::Upper, uh)?;
        Ok((h, size, is_dir))
    }
}

impl Provider for OverlayProvider {
    fn capabilities(&self) -> Capabilities {
        // A writable upper makes the stack writable regardless of the base,
        // and a stack you can write to is by definition not immutable —
        // declaring otherwise would be a promise a caching layer would act
        // on. `slow` and `preferred_block` still combine across both
        // children.
        Capabilities {
            access: Access::ReadWrite,
            immutable: false,
            ..Capabilities::weakest([self.base.capabilities(), self.upper.capabilities()])
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        if p.rel.is_empty() {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        if self.hidden_by_whiteout(p)? {
            return Ok(None);
        }
        if let Some(st) = self.upper.getattr(p)? {
            return Ok(Some(st));
        }
        self.base.getattr(p)
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let path = p.rel;
        // Keyed by `vfs_core::fold` throughout — the same fold the shim
        // applies before a vpath crosses the ring. It matters most for the
        // whiteout lookup below: an ASCII-only key means a `.wh.` marker for
        // a non-ASCII-cased name never removes the base entry it names, so a
        // mod-deleted file stays visible.
        let mut map: HashMap<String, DirEntry> = HashMap::new();
        let mut upper_is_dir = false;
        let mut base_is_dir = false;
        // One side reporting "that is not a directory" is a fact about *that
        // side*, not about the merged view. A file sitting in the upper where
        // the base has a directory must cost the caller the upper's
        // contribution, not the entire listing — for a game's `Data`
        // directory the difference is "one stray file is invisible" versus
        // "the game sees no content at all". `MountGraph::readdir` already
        // tolerates it the same way; this used to propagate it and fail the
        // whole call.
        let mut not_dir = false;

        if !self.hidden_by_whiteout(p)? {
            match self.base.readdir(p) {
                Ok(entries) => {
                    base_is_dir = true;
                    map.reserve(entries.len());
                    for e in entries {
                        map.insert(fold(&e.name), e);
                    }
                }
                Err(e) if e == not_found() => {}
                Err(e) if e == not_a_dir() => not_dir = true,
                Err(e) => return Err(e),
            }
        }

        match self.upper.readdir(p) {
            Ok(entries) => {
                upper_is_dir = true;
                for e in entries {
                    if let Some(target) = e.name.strip_prefix(".wh.") {
                        map.remove(&fold(target));
                        continue;
                    }
                    // A crashed copy-up's temp file must never surface as a
                    // visible entry.
                    if e.name.starts_with(".cu.") {
                        continue;
                    }
                    // The upper's entry is the live one, but a name the base
                    // also has keeps the base's spelling (see
                    // `merge_upper_entry`).
                    crate::merge_upper_entry(&mut map, fold(&e.name), e);
                }
            }
            Err(e) if e == not_found() => {}
            Err(e) if e == not_a_dir() => not_dir = true,
            Err(e) => return Err(e),
        }

        if !upper_is_dir && !base_is_dir {
            // Neither side is a directory here, and at least one said so
            // outright. Now — and only now — that is the caller's answer.
            if not_dir {
                return Err(not_a_dir());
            }
            if !path.is_empty() && self.getattr(p)?.is_none() {
                return Err(not_found());
            }
        }

        Ok(crate::sorted_by_folded_name(map))
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        if flags & OPEN_WRITE != 0 {
            return self.open_for_write(p, flags);
        }
        if self.hidden_by_whiteout(p)? {
            return Err(not_found());
        }
        match self.upper.open(p, flags) {
            Ok((uh, size, is_dir)) => {
                let h = self.track(Layer::Upper, uh)?;
                Ok((h, size, is_dir))
            }
            Err(e) if e == not_found() => {
                let (bh, size, is_dir) = self.base.open(p, flags)?;
                let h = self.track(Layer::Base, bh)?;
                Ok((h, size, is_dir))
            }
            Err(e) => Err(e),
        }
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (layer, inner) = self.lookup(h)?;
        match layer {
            Layer::Upper => self.upper.read_at(inner, offset, buf),
            Layer::Base => self.base.read_at(inner, offset, buf),
        }
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let (layer, inner) = self
            .opens
            .lock()
            .map_err(|_| map_io_err())?
            .remove(&h)
            .ok_or_else(bad_fh)?;
        match layer {
            Layer::Upper => self.upper.close(inner),
            Layer::Base => self.base.close(inner),
        }
    }

    /// A handle on a base file is as immutable as the base: base is never
    /// written through this overlay (a write copies the file up and the
    /// *path* then resolves to the upper), so what this handle reads cannot
    /// change while it is open. A handle on the upper never is.
    fn is_immutable(&self, h: Handle) -> bool {
        match self.lookup(h) {
            Ok((Layer::Base, inner)) => self.base.is_immutable(inner),
            _ => false,
        }
    }

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        match self.lookup(h)? {
            (Layer::Upper, inner) => self.upper.write_at(inner, offset, buf),
            (Layer::Base, _) => Err(not_supported()),
        }
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        match self.lookup(h)? {
            (Layer::Upper, inner) => self.upper.set_len(inner, len),
            (Layer::Base, _) => Err(not_supported()),
        }
    }

    fn flush(&self, h: Handle) -> Result<(), i32> {
        match self.lookup(h)? {
            (Layer::Upper, inner) => self.upper.flush(inner),
            (Layer::Base, _) => Err(not_supported()),
        }
    }

    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        // Same reasoning as `open_for_write`: an ancestor's opaque removal
        // is not something a create under it can silently paper over.
        if self.ancestor_whited_out(p)? {
            return Err(not_found());
        }
        self.clear_whiteout(p)?;
        self.upper.mkdir(p)
    }

    fn remove(&self, p: VPath) -> Result<(), i32> {
        if self.hidden_by_whiteout(p)? {
            return Err(not_found());
        }
        let in_upper = self.upper.getattr(p)?.is_some();
        let in_base = self.base.getattr(p)?.is_some();
        if !in_upper && !in_base {
            return Err(not_found());
        }
        // A path copied up earlier and then removed must not let the base
        // version resurface: delete the upper copy (if any) *and* whiteout
        // the base version (if any) rather than treating the two as
        // mutually exclusive.
        if in_upper {
            self.upper.remove(p)?;
        }
        if in_base {
            self.write_whiteout(p)?;
        }
        Ok(())
    }

    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        if from.root != to.root {
            return Err(bad_request());
        }
        if self.hidden_by_whiteout(from)? {
            return Err(not_found());
        }
        if fold(from.rel) == fold(to.rel) {
            // A change of letter case only: the same entry under a new
            // spelling. Nothing moves, so nothing is copied up and — above
            // all — nothing is whited out: the whiteout a real rename leaves
            // at `from` would hide the file itself.
            //
            // The upper respells an entry it holds. One only the base holds
            // keeps the base's spelling, as every name the base has does
            // (see `readdir`); the rename succeeds, as it would on a
            // filesystem that folded the two names together.
            return match self.upper.getattr(from)? {
                Some(_) => self.upper.rename(from, to),
                None if self.base.getattr(from)?.is_some() => Ok(()),
                None => Err(not_found()),
            };
        }
        self.copy_up_if_needed(from)?;
        let from_in_base = self.base.getattr(from)?.is_some();
        self.upper.rename(from, to)?;
        // The destination may have been whited out by an earlier remove;
        // the rename just gave it real content again.
        self.clear_whiteout(to)?;
        if from_in_base {
            self.write_whiteout(from)?;
        }
        Ok(())
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        if self.hidden_by_whiteout(p)? {
            return Err(not_found());
        }
        self.copy_up_if_needed(p)?;
        self.upper.set_attr(p, attr)
    }

    /// The same rule as `readdir`, for one name: the base's spelling if the
    /// base has the name, the upper's otherwise.
    fn stored_name(&self, p: VPath) -> Result<Option<String>, i32> {
        if p.rel.is_empty() || self.hidden_by_whiteout(p)? {
            return Ok(None);
        }
        crate::merge_stored_name(self.base.as_ref(), self.upper.as_ref(), p)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::InlineProvider;
    use vfs_provider::{CaseMatch, OPEN_EXCL, OPEN_READ};

    /// Slow and immutable, but sequential-only — exercises both the
    /// pass-through fields and the forced access/immutable overrides at once.
    struct SlowSeqBase;

    impl Provider for SlowSeqBase {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                access: vfs_provider::Access::SeqRead,
                immutable: true,
                slow: true,
                preferred_block: Some(4096),
                // Never resolves any name (see the stubs below), so
                // fold-equal-resolves-identically holds vacuously.
                case: CaseMatch::Insensitive,
            }
        }
        fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
            Ok(None)
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(Vec::new())
        }
        fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
            Err(not_found())
        }
        fn close(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }
        fn read_at(&self, _h: Handle, _o: u64, _b: &mut [u8]) -> Result<usize, i32> {
            Ok(0)
        }
    }

    /// A genuinely empty in-memory `ReadWrite` provider, for use as an
    /// overlay upper. Deliberately NOT `vfs_provider::RwMemFixture`: that one
    /// is a conformance fixture and is permanently obligated to serve
    /// `FIXTURE_FILES` so it can pass the suite on its own — which means an
    /// overlay built on it would pass its tests even while ignoring its base
    /// entirely. An overlay's upper must start empty.
    ///
    /// `pub(crate)` so `SubdirProvider`'s writable-inner conformance test can
    /// reuse it: `RwMemFixture` always serves `FIXTURE_FILES` at its own
    /// root, which does not fit behind `SubdirProvider`'s path-prefix
    /// rewrite, but a blank writable store that the test can seed under the
    /// prefix itself does.
    #[derive(Default)]
    pub(crate) struct MemUpper {
        files: Mutex<HashMap<String, Vec<u8>>>,
        dirs: Mutex<HashSet<String>>,
        next: AtomicU64,
        opens: Mutex<HashMap<Handle, String>>,
    }

    impl Provider for MemUpper {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                access: Access::ReadWrite,
                immutable: false,
                slow: false,
                preferred_block: None,
                // Exact-keyed HashMap below: byte-exact, not fold-equal.
                case: CaseMatch::Sensitive,
            }
        }

        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            if p.rel.is_empty() {
                return Ok(Some(Stat {
                    kind: KIND_DIR,
                    size: 0,
                    mtime: 0,
                }));
            }
            if let Some(body) = self.files.lock().unwrap().get(p.rel) {
                return Ok(Some(Stat {
                    kind: KIND_FILE,
                    size: body.len() as u64,
                    mtime: 0,
                }));
            }
            if self.dirs.lock().unwrap().contains(p.rel) {
                return Ok(Some(Stat {
                    kind: KIND_DIR,
                    size: 0,
                    mtime: 0,
                }));
            }
            Ok(None)
        }

        fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
            let prefix = if p.rel.is_empty() {
                String::new()
            } else {
                format!("{}/", p.rel)
            };
            let mut seen: HashMap<String, DirEntry> = HashMap::new();
            for (rel, body) in self.files.lock().unwrap().iter() {
                let Some(rest) = rel.strip_prefix(prefix.as_str()) else {
                    continue;
                };
                if rest.is_empty() || rest.contains('/') {
                    continue;
                }
                seen.insert(
                    rest.to_string(),
                    DirEntry {
                        name: rest.to_string(),
                        stat: Stat {
                            kind: KIND_FILE,
                            size: body.len() as u64,
                            mtime: 0,
                        },
                    },
                );
            }
            for d in self.dirs.lock().unwrap().iter() {
                let Some(rest) = d.strip_prefix(prefix.as_str()) else {
                    continue;
                };
                if rest.is_empty() || rest.contains('/') {
                    continue;
                }
                seen.entry(rest.to_string()).or_insert(DirEntry {
                    name: rest.to_string(),
                    stat: Stat {
                        kind: KIND_DIR,
                        size: 0,
                        mtime: 0,
                    },
                });
            }
            if seen.is_empty() && !p.rel.is_empty() {
                return Err(not_found());
            }
            let mut out: Vec<DirEntry> = seen.into_values().collect();
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        }

        fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
            let mut files = self.files.lock().unwrap();
            let exists = files.contains_key(p.rel);
            if flags & OPEN_WRITE == 0 {
                if !exists {
                    return Err(not_found());
                }
            } else {
                if flags & OPEN_EXCL != 0 && exists {
                    return Err(bad_request());
                }
                if flags & OPEN_CREATE != 0 {
                    files.entry(p.rel.to_string()).or_default();
                } else if !exists {
                    return Err(not_found());
                }
                if flags & OPEN_TRUNC != 0 {
                    files.insert(p.rel.to_string(), Vec::new());
                }
            }
            let size = files.get(p.rel).map(|b| b.len()).unwrap_or(0) as u64;
            drop(files);
            let h = self.next.fetch_add(1, Ordering::Relaxed);
            self.opens.lock().unwrap().insert(h, p.rel.to_string());
            Ok((h, size, false))
        }

        fn close(&self, h: Handle) -> Result<(), i32> {
            self.opens.lock().unwrap().remove(&h).ok_or_else(bad_fh)?;
            Ok(())
        }

        fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            let path = self
                .opens
                .lock()
                .unwrap()
                .get(&h)
                .cloned()
                .ok_or_else(bad_fh)?;
            let files = self.files.lock().unwrap();
            let body = files.get(&path).ok_or_else(bad_fh)?;
            let start = (offset as usize).min(body.len());
            let n = (body.len() - start).min(buf.len());
            buf[..n].copy_from_slice(&body[start..start + n]);
            Ok(n)
        }

        fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
            let path = self
                .opens
                .lock()
                .unwrap()
                .get(&h)
                .cloned()
                .ok_or_else(bad_fh)?;
            let mut files = self.files.lock().unwrap();
            let body = files.entry(path).or_default();
            let end = offset as usize + buf.len();
            if body.len() < end {
                body.resize(end, 0);
            }
            body[offset as usize..end].copy_from_slice(buf);
            Ok(buf.len())
        }

        fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
            let path = self
                .opens
                .lock()
                .unwrap()
                .get(&h)
                .cloned()
                .ok_or_else(bad_fh)?;
            self.files
                .lock()
                .unwrap()
                .entry(path)
                .or_default()
                .resize(len as usize, 0);
            Ok(())
        }

        fn flush(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }

        fn mkdir(&self, p: VPath) -> Result<(), i32> {
            self.dirs.lock().unwrap().insert(p.rel.to_string());
            Ok(())
        }

        /// Refuses a directory that still holds something, with `ST_IS_DIR`.
        ///
        /// This fixture used to drop the `dirs` entry and answer `Ok(())` with
        /// the children untouched, which is the same silent no-op
        /// `MemoryProvider` had — and `OverlayProvider::remove` propagates its
        /// upper's answer verbatim, so the overlay inherited it. The shared
        /// conformance suite's non-empty-directory case is what surfaced it.
        fn remove(&self, p: VPath) -> Result<(), i32> {
            let mut files = self.files.lock().unwrap();
            let mut dirs = self.dirs.lock().unwrap();
            if files.remove(p.rel).is_some() {
                return Ok(());
            }
            let prefix = if p.rel.is_empty() {
                String::new()
            } else {
                format!("{}/", p.rel)
            };
            if files.keys().any(|k| k.starts_with(&prefix))
                || dirs.iter().any(|d| d.starts_with(&prefix))
            {
                return Err(vfs_provider::is_dir());
            }
            if dirs.remove(p.rel) {
                return Ok(());
            }
            Err(not_found())
        }

        fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
            if from.root != to.root {
                return Err(bad_request());
            }
            let mut files = self.files.lock().unwrap();
            let body = files.remove(from.rel).ok_or_else(not_found)?;
            files.insert(to.rel.to_string(), body);
            Ok(())
        }

        fn set_attr(&self, _p: VPath, _attr: SetAttr) -> Result<(), i32> {
            Ok(())
        }
    }

    /// Wraps a `Provider` and counts calls to `open`, so a test can assert a
    /// piece of code touched the wrapped provider exactly N times. A
    /// final-size or final-content check on a copy-up race can't distinguish
    /// "one thread copied" from "eight threads copied the same bytes" — this
    /// can.
    struct CountingOpens<P> {
        inner: P,
        opens: AtomicU64,
    }

    impl<P> CountingOpens<P> {
        fn new(inner: P) -> Self {
            CountingOpens {
                inner,
                opens: AtomicU64::new(0),
            }
        }
    }

    impl<P: Provider> Provider for CountingOpens<P> {
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            self.inner.getattr(p)
        }
        fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
            self.inner.readdir(p)
        }
        fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            self.inner.open(p, flags)
        }
        fn close(&self, h: Handle) -> Result<(), i32> {
            self.inner.close(h)
        }
        fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            self.inner.read_at(h, offset, buf)
        }
    }

    /// Counts the calls an overlay makes into its **upper**, which is where
    /// the whiteout bookkeeping lands. A correctness test cannot see the cost
    /// of that bookkeeping at all — the answers are identical either way —
    /// so this is the only thing that can hold the read path to a budget.
    #[derive(Default)]
    struct CountingUpper {
        inner: MemUpper,
        getattrs: AtomicU64,
        readdirs: AtomicU64,
        /// Runs once, after the next `readdir` has read the directory and
        /// before it returns: what another thread could do in that window.
        #[allow(clippy::type_complexity)]
        after_readdir: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }

    impl Provider for CountingUpper {
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            self.getattrs.fetch_add(1, Ordering::Relaxed);
            self.inner.getattr(p)
        }
        fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
            self.readdirs.fetch_add(1, Ordering::Relaxed);
            let entries = self.inner.readdir(p);
            let hook = self.after_readdir.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
            entries
        }
        fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
            self.inner.open(p, flags)
        }
        fn close(&self, h: Handle) -> Result<(), i32> {
            self.inner.close(h)
        }
        fn read_at(&self, h: Handle, o: u64, b: &mut [u8]) -> Result<usize, i32> {
            self.inner.read_at(h, o, b)
        }
        fn write_at(&self, h: Handle, o: u64, b: &[u8]) -> Result<usize, i32> {
            self.inner.write_at(h, o, b)
        }
        fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
            self.inner.set_len(h, len)
        }
        fn flush(&self, h: Handle) -> Result<(), i32> {
            self.inner.flush(h)
        }
        fn mkdir(&self, p: VPath) -> Result<(), i32> {
            self.inner.mkdir(p)
        }
        fn remove(&self, p: VPath) -> Result<(), i32> {
            self.inner.remove(p)
        }
        fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
            self.inner.rename(from, to)
        }
        fn set_attr(&self, p: VPath, a: SetAttr) -> Result<(), i32> {
            self.inner.set_attr(p, a)
        }
    }

    /// A base whose `read_at` succeeds for the first chunk of a file and then
    /// fails every call after — used to prove that a copy-up which dies
    /// partway through a multi-chunk file leaves no trace in upper, rather
    /// than a truncated destination that a later check would mistake for a
    /// complete copy.
    struct FlakyReadBase {
        body: Vec<u8>,
    }

    impl Provider for FlakyReadBase {
        fn capabilities(&self) -> Capabilities {
            // getattr below compares `p.rel` to "big.bin" by byte equality.
            Capabilities { case: CaseMatch::Sensitive, ..Capabilities::read_only() }
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            if p.rel.is_empty() {
                return Ok(Some(Stat {
                    kind: KIND_DIR,
                    size: 0,
                    mtime: 0,
                }));
            }
            if p.rel == "big.bin" {
                return Ok(Some(Stat {
                    kind: KIND_FILE,
                    size: self.body.len() as u64,
                    mtime: 0,
                }));
            }
            Ok(None)
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(vec![DirEntry {
                name: "big.bin".to_string(),
                stat: Stat {
                    kind: KIND_FILE,
                    size: self.body.len() as u64,
                    mtime: 0,
                },
            }])
        }
        fn open(&self, p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
            if p.rel == "big.bin" {
                Ok((1, self.body.len() as u64, false))
            } else {
                Err(not_found())
            }
        }
        fn close(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }
        fn read_at(&self, _h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            // First chunk (copy_loop's buffer is 64 KiB) succeeds; anything
            // after that fails, simulating a read that dies partway through
            // a multi-chunk file.
            if offset >= 65536 {
                return Err(map_io_err());
            }
            let start = offset as usize;
            let n = (self.body.len() - start).min(buf.len());
            buf[..n].copy_from_slice(&self.body[start..start + n]);
            Ok(n)
        }
    }

    #[test]
    fn overlay_reports_read_write_and_is_never_immutable() {
        let ov = OverlayProvider::new(Arc::new(SlowSeqBase), MemUpper::default()).unwrap();
        let caps = ov.capabilities();
        assert_eq!(
            caps.access,
            vfs_provider::Access::ReadWrite,
            "a writable upper makes the whole stack writable regardless of the base"
        );
        // A stack you can write to is by definition not immutable: claiming
        // otherwise would be a promise a caching layer would act on, and
        // Capabilities::validate() rejects ReadWrite + immutable as the
        // self-contradiction it is. Do not "fix" this back to true.
        assert!(!caps.immutable, "a writable stack can never be immutable");
        assert!(caps.slow, "slow still derives from the children");
        assert_eq!(caps.preferred_block, Some(4096));
    }

    #[test]
    fn overlay_over_the_fixture_tree_with_an_empty_upper_passes_conformance() {
        let base: Arc<dyn Provider> = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let p: Arc<dyn Provider> =
            Arc::new(OverlayProvider::new(base, MemUpper::default()).unwrap());
        vfs_provider::assert_conformance(p);
    }

    #[test]
    fn upper_wins_and_whiteout_hides() {
        let base = Arc::new(InlineProvider::from_files([
            ("a.txt", b"BASE".as_slice()),
            ("gone.txt", b"X".as_slice()),
        ]));
        let upper = MemUpper::default();
        let (h, _, _) = upper
            .open(VPath::at_default("a.txt"), OPEN_WRITE | OPEN_CREATE)
            .unwrap();
        upper.write_at(h, 0, b"UPPER").unwrap();
        upper.close(h).unwrap();
        let (h, _, _) = upper
            .open(VPath::at_default(".wh.gone.txt"), OPEN_WRITE | OPEN_CREATE)
            .unwrap();
        upper.close(h).unwrap();
        let ov = OverlayProvider::new(base, upper).unwrap();

        let st = ov.getattr(VPath::at_default("a.txt")).unwrap().unwrap();
        assert_eq!(st.size, 5);
        assert!(ov.getattr(VPath::at_default("gone.txt")).unwrap().is_none());

        let (h, _, _) = ov.open(VPath::at_default("a.txt"), OPEN_READ).unwrap();
        let mut buf = [0u8; 8];
        let n = ov.read_at(h, 0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"UPPER");
        ov.close(h).unwrap();
    }

    #[test]
    fn overlay_declares_read_write_over_a_read_only_base() {
        use vfs_provider::{Access, Provider};
        let base = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let ov = OverlayProvider::new(base, MemUpper::default()).unwrap();
        assert_eq!(ov.capabilities().access, Access::ReadWrite);
    }

    #[test]
    fn overlay_rejects_a_read_only_upper_at_construction() {
        let base = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let upper = InlineProvider::from_files(std::iter::empty::<(&str, &[u8])>());
        assert!(
            OverlayProvider::new(base, upper).is_err(),
            "a read-only upper must be refused at construction, not at first write"
        );
    }

    #[test]
    fn writing_a_base_file_copies_it_up_and_leaves_base_untouched() {
        use vfs_provider::{Provider, VPath, OPEN_READ, OPEN_WRITE};
        let base = Arc::new(InlineProvider::from_files([("a.txt", b"BASE".as_slice())]));
        let ov = OverlayProvider::new(base.clone(), MemUpper::default()).unwrap();

        let f = VPath::at_default("a.txt");
        let (h, _, _) = ov.open(f, OPEN_WRITE).expect("open for write copies up");
        ov.write_at(h, 0, b"UP").expect("write");
        ov.close(h).expect("close");

        let (h, _, _) = ov.open(f, OPEN_READ).expect("reopen");
        let mut buf = [0u8; 8];
        let n = ov.read_at(h, 0, &mut buf).expect("read");
        assert_eq!(&buf[..n], b"UPSE", "copy-up must preserve the untouched tail");
        ov.close(h).expect("close");

        // The base is never mutated.
        let (bh, _, _) = base.open(f, OPEN_READ).expect("base open");
        let n = base.read_at(bh, 0, &mut buf).expect("base read");
        assert_eq!(&buf[..n], b"BASE", "copy-up mutated the base");
        base.close(bh).unwrap();
    }

    #[test]
    fn removing_a_base_file_writes_a_whiteout() {
        use vfs_provider::{Provider, VPath};
        let base = Arc::new(InlineProvider::from_files([("a.txt", b"BASE".as_slice())]));
        let ov = OverlayProvider::new(base, MemUpper::default()).unwrap();
        let f = VPath::at_default("a.txt");
        ov.remove(f).expect("remove");
        assert!(
            ov.getattr(f).expect("getattr").is_none(),
            "whiteout did not hide the base file"
        );
        assert!(
            !ov.readdir(VPath::at_default(""))
                .expect("readdir")
                .iter()
                .any(|e| e.name == "a.txt"),
            "whiteout did not hide the entry from readdir"
        );
    }

    #[test]
    fn concurrent_opens_copy_up_exactly_once() {
        use std::sync::Arc as StdArc;
        use vfs_provider::{Provider, VPath, OPEN_WRITE};
        let counted = Arc::new(CountingOpens::new(InlineProvider::from_files([(
            "a.txt",
            b"BASE".as_slice(),
        )])));
        let base: Arc<dyn Provider> = counted.clone();
        let ov: StdArc<OverlayProvider> =
            StdArc::new(OverlayProvider::new(base, MemUpper::default()).unwrap());

        let mut hs = Vec::new();
        for _ in 0..8 {
            let ov = StdArc::clone(&ov);
            hs.push(std::thread::spawn(move || {
                let (h, _, _) = ov.open(VPath::at_default("a.txt"), OPEN_WRITE).expect("open");
                ov.close(h).expect("close");
            }));
        }
        for h in hs {
            h.join().expect("thread");
        }
        // Content must still be the base content, not a truncated or doubled copy.
        let (h, size, _) = ov.open(VPath::at_default("a.txt"), vfs_provider::OPEN_READ).unwrap();
        assert_eq!(size, 4, "concurrent copy-up corrupted the file");
        ov.close(h).unwrap();

        // The assertion that actually proves exclusivity: every racing
        // thread reads/writes identical bytes, so the size check above would
        // pass just the same if all eight threads had copied concurrently.
        // Counting how many times the base was opened does not.
        assert_eq!(
            counted.opens.load(Ordering::Relaxed),
            1,
            "copy-up opened the base more than once — the in-flight lock did not serialize the race"
        );
    }

    /// Gate 4, Task 6 review. The whiteout check runs on **every** read, and
    /// the obvious implementation costs one `upper.getattr` per ancestor —
    /// so a five-deep asset path pays six filesystem calls to answer a
    /// question whose answer is "no" for the entire session. At a game load's
    /// volume that is six figures of syscalls, in a harness whose other job
    /// is measuring load time.
    ///
    /// The budget asserted here is the whole point of the index: **one**
    /// `upper.getattr` per warm `getattr` (the real content lookup, which was
    /// always there), and **zero** `upper.readdir`. Correctness tests cannot
    /// see this — the answers are the same either way.
    #[test]
    fn warm_reads_cost_one_upper_lookup_regardless_of_path_depth() {
        use vfs_provider::{Provider, VPath};
        let base = Arc::new(InlineProvider::from_files([
            ("a/b/c/d/deep.txt", b"DEEP".as_slice()),
            ("a/b/c/d/sibling.txt", b"SIB".as_slice()),
            ("shallow.txt", b"TOP".as_slice()),
        ]));
        let upper = Arc::new(CountingUpper::default());
        let ov = OverlayProvider::from_arcs(base, upper.clone()).unwrap();

        let deep = VPath::at_default("a/b/c/d/deep.txt");
        // Warm-up: this is where the per-directory scans happen, once.
        assert!(ov.getattr(deep).unwrap().is_some());
        let warm_getattrs = upper.getattrs.load(Ordering::Relaxed);
        let warm_readdirs = upper.readdirs.load(Ordering::Relaxed);
        assert!(
            warm_readdirs <= 5,
            "warm-up must scan at most one directory per path component \
             (5 for a 5-deep path), got {warm_readdirs}"
        );

        // Now the steady state: repeat reads, plus a sibling and an unrelated
        // shallow path, both of which reuse directories already scanned.
        for _ in 0..20 {
            assert!(ov.getattr(deep).unwrap().is_some());
        }
        assert!(ov
            .getattr(VPath::at_default("a/b/c/d/sibling.txt"))
            .unwrap()
            .is_some());
        assert!(ov.getattr(VPath::at_default("shallow.txt")).unwrap().is_some());
        // A path that does not exist anywhere must not reopen the question
        // either.
        assert!(ov
            .getattr(VPath::at_default("a/b/c/d/absent.txt"))
            .unwrap()
            .is_none());

        assert_eq!(
            upper.readdirs.load(Ordering::Relaxed),
            warm_readdirs,
            "a warm read must not touch the upper's directories at all; every call here \
             walks directories the index already holds"
        );
        assert_eq!(
            upper.getattrs.load(Ordering::Relaxed) - warm_getattrs,
            23,
            "each warm read must cost exactly one `upper.getattr` — the content lookup that \
             was always there — and none for the whiteout walk. A number near 6x this is the \
             per-ancestor `metadata` storm the index removes"
        );
    }

    /// The whiteout walk folds a path once and splits the fold at the same
    /// separators as the path. That only works because a fold never adds or
    /// removes a `/` — including for names whose fold changes length (`İ`,
    /// two bytes, folds to three) or turns a non-ASCII character into an
    /// ASCII one (the Kelvin sign folds to `k`). Markers on such names, at
    /// every depth, must hide exactly what they hid when each ancestor was
    /// folded on its own.
    #[test]
    fn whiteouts_hide_through_folds_that_change_a_names_length() {
        let base = Arc::new(InlineProvider::from_files([
            ("İstanbul/Sub/a.txt", b"1".as_slice()),
            ("İstanbul/Sub/keep.txt", b"2".as_slice()),
            ("İstanbul/other/b.txt", b"3".as_slice()),
            ("top/\u{212A}elvin/deep/c.txt", b"4".as_slice()),
            ("top/\u{212A}elvin/d.txt", b"5".as_slice()),
            ("top/plain/E.TXT", b"6".as_slice()),
            ("ÄÖ/ü.txt", b"7".as_slice()),
        ]));
        let upper = MemUpper::default();
        // Markers spelled in a different case from the names they hide: one
        // on a file two levels down, one on a directory, one at the root.
        // (The directories are spelled as the base spells them: the test
        // upper is case-sensitive, and a directory's markers are read by the
        // spelling of the first lookup through it.)
        for marker in ["İstanbul/Sub/.wh.A.TXT", "top/.wh.kELVIN", ".wh.äö"] {
            let (h, _, _) = upper
                .open(VPath::at_default(marker), OPEN_WRITE | OPEN_CREATE)
                .unwrap();
            upper.close(h).unwrap();
        }
        let ov = OverlayProvider::new(base, upper).unwrap();
        let seen = |p: &str| ov.getattr(VPath::at_default(p)).unwrap().is_some();
        let opens = |p: &str| match ov.open(VPath::at_default(p), OPEN_READ) {
            Ok((h, _, _)) => {
                ov.close(h).unwrap();
                true
            }
            Err(e) => {
                assert_eq!(e, not_found());
                false
            }
        };
        // Twice: the first pass scans each directory, the second is answered
        // from the index.
        for _ in 0..2 {
            for (path, visible) in [
                ("İstanbul/Sub/a.txt", false),
                ("i\u{307}stanbul/sub/A.TXT", false),
                ("İstanbul/Sub/keep.txt", true),
                ("İstanbul/Sub", true),
                ("İstanbul/other/b.txt", true),
                ("top/\u{212A}elvin", false),
                ("top/\u{212A}elvin/d.txt", false),
                ("top/\u{212A}elvin/deep/c.txt", false),
                ("TOP/kelvin/DEEP/C.TXT", false),
                ("top/plain/E.TXT", true),
                ("top", true),
                ("ÄÖ", false),
                ("ÄÖ/ü.txt", false),
                ("äö/Ü.TXT", false),
            ] {
                assert_eq!(seen(path), visible, "getattr {path:?}");
                // (Files only: the test providers do not open directories.)
                if path.to_lowercase().ends_with(".txt") {
                    assert_eq!(opens(path), visible, "open {path:?}");
                }
            }
        }
        // A directory under a whited-out one lists as gone; its sibling lists.
        assert_eq!(
            ov.readdir(VPath::at_default("top/\u{212A}elvin/deep")),
            Err(not_found())
        );
        let names: Vec<String> = ov
            .readdir(VPath::at_default("top"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["plain"]);
    }

    /// The three questions the walk answers keep their separate meanings: a
    /// marker on the path itself is not an ancestor's, and an ancestor's is
    /// not the path's own.
    #[test]
    fn the_whiteout_walk_tells_a_paths_own_marker_from_an_ancestors() {
        let base = Arc::new(InlineProvider::from_files([
            ("a/b/c.txt", b"1".as_slice()),
            ("x/y/z.txt", b"2".as_slice()),
            ("top.txt", b"3".as_slice()),
        ]));
        let upper = MemUpper::default();
        for marker in ["a/b/.wh.c.txt", ".wh.x", ".wh.top.txt"] {
            let (h, _, _) = upper
                .open(VPath::at_default(marker), OPEN_WRITE | OPEN_CREATE)
                .unwrap();
            upper.close(h).unwrap();
        }
        let ov = OverlayProvider::new(base, upper).unwrap();
        let ask = |p: &str| {
            let p = VPath::at_default(p);
            (
                ov.is_whiteout(p).unwrap(),
                ov.ancestor_whited_out(p).unwrap(),
                ov.hidden_by_whiteout(p).unwrap(),
            )
        };
        for _ in 0..2 {
            assert_eq!(ask("a/b/c.txt"), (true, false, true));
            assert_eq!(ask("a/b"), (false, false, false));
            assert_eq!(ask("a/b/other.txt"), (false, false, false));
            assert_eq!(ask("x"), (true, false, true));
            assert_eq!(ask("x/y"), (false, true, true));
            assert_eq!(ask("x/y/z.txt"), (false, true, true));
            assert_eq!(ask("top.txt"), (true, false, true));
            assert_eq!(ask("absent"), (false, false, false));
            assert_eq!(ask(""), (false, false, false));
        }
    }

    /// A directory's first scan runs outside the index lock. A whiteout
    /// written in that window is in neither the scan (already read) nor the
    /// index (the directory was not scanned yet, so there was no entry to
    /// update); storing the scan afterwards would leave the removed file
    /// visible to `getattr` and `open` for good. The remove is run from
    /// inside the scan here, so the window is hit every time.
    #[test]
    fn a_whiteout_written_during_a_directorys_first_scan_still_hides() {
        use vfs_provider::{Provider, VPath};
        for clear in [false, true] {
            let base = Arc::new(InlineProvider::from_files([
                ("dir/a.txt", b"BASE".as_slice()),
                ("dir/b.txt", b"KEEP".as_slice()),
            ]));
            let upper = Arc::new(CountingUpper::default());
            let a = VPath::at_default("dir/a.txt");
            if clear {
                // The mirror image: the marker exists, and is cleared (by a
                // create over it) while the scan that saw it is in flight.
                let (h, _, _) = upper
                    .open(VPath::at_default("dir/.wh.a.txt"), OPEN_WRITE | OPEN_CREATE)
                    .unwrap();
                upper.close(h).unwrap();
            }
            let ov = Arc::new(OverlayProvider::from_arcs(base, upper.clone()).unwrap());
            let other = Arc::clone(&ov);
            *upper.after_readdir.lock().unwrap() = Some(Box::new(move || {
                if clear {
                    let (h, _, _) = other.open(a, OPEN_WRITE | OPEN_CREATE).unwrap();
                    other.close(h).unwrap();
                } else {
                    other.remove(a).unwrap();
                }
            }));
            // This lookup scans `dir`; the change lands mid-scan.
            let _ = ov.getattr(a).unwrap();
            assert!(
                upper.after_readdir.lock().unwrap().is_none(),
                "the scan ran"
            );
            // Whatever that lookup answered, the change is complete now.
            assert_eq!(ov.getattr(a).unwrap().is_some(), clear, "clear={clear}");
            assert_eq!(
                ov.open(a, OPEN_READ).map(|(h, _, _)| ov.close(h).unwrap()),
                if clear { Ok(()) } else { Err(not_found()) },
                "clear={clear}"
            );
            assert!(ov
                .getattr(VPath::at_default("dir/b.txt"))
                .unwrap()
                .is_some());
            // Break the test's reference cycle (upper → hook → overlay).
            drop(ov);
        }
    }

    /// The index is only sound if it tracks the markers this provider writes.
    /// A whiteout created *after* its directory was scanned must hide, and
    /// clearing it must un-hide — otherwise the cache is a correctness bug
    /// wearing a performance fix.
    #[test]
    fn a_whiteout_written_after_its_directory_was_scanned_still_hides() {
        use vfs_provider::{Provider, VPath, OPEN_CREATE, OPEN_WRITE};
        let base = Arc::new(InlineProvider::from_files([("dir/a.txt", b"BASE".as_slice())]));
        let ov = OverlayProvider::new(base, MemUpper::default()).unwrap();
        let f = VPath::at_default("dir/a.txt");

        // Read first, so "dir" and the root are already scanned and cached as
        // holding no markers.
        assert!(ov.getattr(f).unwrap().is_some());
        assert!(!ov.readdir(VPath::at_default("dir")).unwrap().is_empty());

        ov.remove(f).expect("remove writes a whiteout");
        assert!(
            ov.getattr(f).unwrap().is_none(),
            "a whiteout written after the directory was scanned did not hide the base file — \
             the index went stale"
        );

        // …and clearing it puts the path back.
        let (h, _, _) = ov.open(f, OPEN_WRITE | OPEN_CREATE).expect("recreate");
        ov.close(h).unwrap();
        assert!(
            ov.getattr(f).unwrap().is_some(),
            "clearing the whiteout did not remove it from the index"
        );
    }

    /// A caller can create a file literally named `.wh.x` through this
    /// provider. The module docs reserve the prefix, so that file *is* a
    /// marker for `x` — and `readdir`, which scans the upper live, treats it
    /// as one. The index has to agree, or the two views of the same directory
    /// disagree about whether `x` is hidden.
    #[test]
    fn creating_a_marker_named_file_through_the_overlay_is_seen_by_the_index() {
        use vfs_provider::{Provider, VPath, OPEN_CREATE, OPEN_WRITE};
        let base = Arc::new(InlineProvider::from_files([("dir/x.txt", b"BASE".as_slice())]));
        let ov = OverlayProvider::new(base, MemUpper::default()).unwrap();

        // Scan "dir" while it holds no markers.
        assert!(ov.getattr(VPath::at_default("dir/x.txt")).unwrap().is_some());

        let (h, _, _) = ov
            .open(VPath::at_default("dir/.wh.x.txt"), OPEN_WRITE | OPEN_CREATE)
            .expect("create a file whose name happens to be a marker");
        ov.close(h).unwrap();

        let listed = ov.readdir(VPath::at_default("dir")).unwrap();
        let listed_x = listed.iter().any(|e| e.name == "x.txt");
        let stat_x = ov.getattr(VPath::at_default("dir/x.txt")).unwrap().is_some();
        assert_eq!(
            listed_x, stat_x,
            "readdir and getattr disagree about whether `x.txt` is hidden: listed={listed_x}, \
             stat={stat_x}. readdir scans the upper live and the index must match it."
        );
    }

    #[test]
    fn overlay_passes_write_conformance() {
        let base = Arc::new(InlineProvider::from_files(
            vfs_provider::FIXTURE_FILES.iter().copied(),
        ));
        let ov: Arc<dyn vfs_provider::Provider> =
            Arc::new(OverlayProvider::new(base, MemUpper::default()).unwrap());
        vfs_provider::assert_conformance(ov);
    }

    #[test]
    fn a_failed_copy_up_leaves_the_destination_absent_not_truncated() {
        use vfs_provider::{Provider, VPath, OPEN_CREATE, OPEN_WRITE};
        let base = Arc::new(FlakyReadBase {
            body: vec![7u8; 200_000],
        });
        let ov = OverlayProvider::new(base, MemUpper::default()).unwrap();
        let f = VPath::at_default("big.bin");

        let err = ov
            .open(f, OPEN_WRITE)
            .expect_err("copy-up must fail when the base read dies partway through");
        assert_eq!(err, map_io_err());

        // The assertion that actually catches the bug: a truncated
        // destination is worse than an error, because every future
        // getattr/copy-up check would see "already present" and treat a
        // half-copied file as fully copied forever after. Checking only the
        // error status above would pass even with that bug intact.
        assert!(
            ov.upper.getattr(f).unwrap().is_none(),
            "a failed copy-up left a truncated file at the destination instead of nothing"
        );
        // And no orphaned `.cu.` temp file should linger either.
        assert!(
            ov.upper
                .readdir(VPath::at_default(""))
                .unwrap()
                .is_empty(),
            "a failed copy-up left a stray temp file behind in upper"
        );

        // Retrying after the transient failure is cleared works normally —
        // the failed attempt left the path genuinely untouched, not stuck.
        let ov2 = OverlayProvider::new(
            Arc::new(InlineProvider::from_files([("big.bin", b"ok".as_slice())])),
            MemUpper::default(),
        )
        .unwrap();
        let (h, _, _) = ov2.open(f, OPEN_WRITE | OPEN_CREATE).expect("unrelated retry works");
        ov2.close(h).unwrap();
    }

    #[test]
    fn creating_under_a_removed_ancestor_directory_is_refused_until_mkdir_recreates_it() {
        use vfs_provider::{Provider, VPath, OPEN_CREATE, OPEN_WRITE};
        let base = Arc::new(InlineProvider::from_files([("dir/a.txt", b"BASE".as_slice())]));
        let ov = OverlayProvider::new(base, MemUpper::default()).unwrap();

        // Opaquely remove the whole base directory.
        ov.remove(VPath::at_default("dir")).expect("whiteout the base directory");
        assert!(ov.getattr(VPath::at_default("dir/a.txt")).unwrap().is_none());

        // Creating a brand-new file underneath the removed directory is
        // refused outright, even with OPEN_CREATE. Clearing the ancestor's
        // whiteout here would silently resurrect every other base entry
        // under "dir" that nobody asked to restore; creating the file while
        // leaving the ancestor whiteout in place would leave it permanently
        // invisible to getattr while still surfacing through readdir's
        // upper merge. Refusing is the only option with no inconsistent
        // state; the way back is explicit.
        let err = ov
            .open(VPath::at_default("dir/new.txt"), OPEN_WRITE | OPEN_CREATE)
            .expect_err("create under a whited-out ancestor must be refused");
        assert_eq!(err, vfs_provider::not_found());

        // The explicit way back: mkdir clears exactly "dir"'s own whiteout.
        ov.mkdir(VPath::at_default("dir")).expect("mkdir recreates the directory");
        let (h, _, _) = ov
            .open(VPath::at_default("dir/new.txt"), OPEN_WRITE | OPEN_CREATE)
            .expect("create succeeds once the ancestor is explicitly recreated");
        ov.close(h).unwrap();
        assert!(ov.getattr(VPath::at_default("dir/new.txt")).unwrap().is_some());
    }
}
