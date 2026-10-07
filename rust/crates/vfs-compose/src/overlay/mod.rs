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
    bad_fh, copy_up_name, whiteout_name, COPY_UP_PREFIX, WHITEOUT_PREFIX, bad_request, is_dir, map_io_err, not_a_dir, not_found, not_supported, Access,
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
            Some((parent, name)) => format!("{parent}/{}", whiteout_name(name)),
            None => whiteout_name(path),
        }
    }

    /// `.cu.<n>.<name>` sibling of `path`, in the same directory as the
    /// eventual destination — so the final rename stays within one parent
    /// (and, for a disk-backed upper, one volume) and is atomic.
    fn temp_copy_path(&self, path: &str, n: u64) -> String {
        match path.rsplit_once('/') {
            Some((parent, name)) => format!("{parent}/{}", copy_up_name(n, name)),
            None => copy_up_name(n, path),
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
                    if let Some(target) = e.name.strip_prefix(WHITEOUT_PREFIX) {
                        map.remove(&fold(target));
                        continue;
                    }
                    // A crashed copy-up's temp file must never surface as a
                    // visible entry.
                    if e.name.starts_with(COPY_UP_PREFIX) {
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
pub(crate) mod tests;
