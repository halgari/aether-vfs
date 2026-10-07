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

mod copy_up;
mod whiteout;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use vfs_core::fold;
use vfs_provider::{
    bad_fh, bad_request, map_io_err, not_a_dir, not_found, not_supported, Access,
    Capabilities, DirEntry, Handle, Provider, SetAttr, Stat, VPath, KIND_DIR,
    OPEN_WRITE, COPY_UP_PREFIX, WHITEOUT_PREFIX,
};

use whiteout::WhiteoutIndex;

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
