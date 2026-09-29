//! [`LayerProvider`]: a named, persistent, read-write namespace in the block
//! store (spec §5). It plugs in wherever a `DiskProvider` write layer goes —
//! typically as the upper of `vfs_compose::OverlayProvider`.
//!
//! The namespace is the catalog's `entries` rows for the layer, keyed by the
//! `vfs_core::fold`ed path with the original spelling in [`EntryRec::name`].
//! File data lives in the store under [`crate::layer_file_id`] of the file's
//! GUID; per-file state and the block read-modify-write are in
//! [`crate::layer_io`].
//!
//! **Namespace lock.** Catalog calls are individually atomic, sequences are
//! not, so every get→put, children→remove or get→rename sequence runs under
//! the layer's `ns` lock. A cell's `state` lock may be held when `ns` is taken,
//! never the reverse (see `layer_io`).
//!
//! **Parents.** Creating a file (or a directory, or a rename destination)
//! creates missing parent directories, as `DiskProvider`'s `create_dir_all`
//! does. The overlay above relies on it: a copy-up of `sub/b.txt` into an
//! upper that has no `sub` writes `sub/.cu.N.b.txt` there first.
//!
//! **Durability (spec §6).** A handle's `flush`, and `close` of a handle that
//! wrote, commit its file, then `BlockStore::flush()`, then the catalog's
//! durable commit — under `ns`, so no other file's row update can slip into
//! the durable catalog commit without its blocks being in the store flush.
//! Namespace changes are non-durable until the next such point. A file whose
//! row is removed or replaced is deleted from the store only after a durable
//! point has made the row's removal durable (catalog first, store second), and
//! only once no handle has it open.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use vfs_core::fold;
use vfs_provider::{
    bad_fh, bad_request, exists, is_dir, map_io_err, not_a_dir, not_found, Access, Capabilities,
    CaseMatch, DirEntry, Handle, Provider, SetAttr, Stat, VPath, KIND_DIR, KIND_FILE, OPEN_CREATE,
    OPEN_EXCL, OPEN_TRUNC, OPEN_WRITE,
};

use crate::catalog::EntryRec;
use crate::ids::{layer_file_id, new_guid, Guid};
use crate::layer_io::{FileCell, FileState};
use crate::storage::{Storage, StorageError};

fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>, i32> {
    m.lock().map_err(|_| map_io_err())
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// A request path, split into components in their original spelling.
struct LPath {
    parts: Vec<String>,
    folded: String,
}

impl LPath {
    /// Backslashes as slashes, empty components dropped; `.` and `..` are
    /// refused rather than walked.
    fn parse(rel: &str) -> Result<Self, i32> {
        let s = rel.replace('\\', "/");
        let parts: Vec<String> = s
            .split('/')
            .filter(|c| !c.is_empty())
            .map(str::to_owned)
            .collect();
        if parts.iter().any(|c| c == "." || c == "..") {
            return Err(bad_request());
        }
        let folded = fold(&parts.join("/"));
        Ok(LPath { parts, folded })
    }

    fn is_root(&self) -> bool {
        self.parts.is_empty()
    }

    fn name(&self) -> &str {
        self.parts.last().map_or("", String::as_str)
    }
}

/// One open handle.
struct OpenFile {
    /// `None` for a directory handle.
    cell: Option<Arc<FileCell>>,
    /// The handle created, truncated, wrote or resized its file: its close
    /// is a durable point.
    wrote: AtomicBool,
}

pub(crate) struct LayerProvider {
    storage: Arc<Storage>,
    name: String,
    id: u64,
    ns: Mutex<()>,
    cells: Mutex<HashMap<Guid, Weak<FileCell>>>,
    handles: Mutex<HashMap<Handle, Arc<OpenFile>>>,
    next: AtomicU64,
    /// GUIDs whose rows are gone and that no handle has open: deleted from the
    /// store at the next durable point.
    doomed: Mutex<Vec<Guid>>,
    /// Test hook: the next file create fails at the store.
    #[cfg(test)]
    pub(crate) fail_store_create: AtomicBool,
}

impl LayerProvider {
    pub(crate) fn new(storage: Arc<Storage>, name: String, id: u64) -> Self {
        LayerProvider {
            storage,
            name,
            id,
            ns: Mutex::new(()),
            cells: Mutex::new(HashMap::new()),
            handles: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
            doomed: Mutex::new(Vec::new()),
            #[cfg(test)]
            fail_store_create: AtomicBool::new(false),
        }
    }

    /// The storage this layer lives in.
    #[cfg(test)]
    pub(crate) fn storage(&self) -> &Storage {
        &self.storage
    }

    fn st_err(&self, what: &str, e: StorageError) -> i32 {
        let status = e.to_status();
        if status == vfs_provider::ST_IO_ERROR {
            tracing::error!(layer = %self.name, error = %e, "layer {what} failed");
        }
        status
    }

    fn get(&self, folded: &str) -> Result<Option<EntryRec>, i32> {
        self.storage
            .catalog
            .get(self.id, folded)
            .map_err(|e| self.st_err("catalog get", e))
    }

    fn put(&self, folded: &str, rec: &EntryRec) -> Result<(), i32> {
        self.storage
            .catalog
            .put(self.id, folded, rec, false)
            .map_err(|e| self.st_err("catalog put", e))
    }

    fn live_cell(&self, guid: &Guid) -> Option<Arc<FileCell>> {
        self.cells.lock().ok()?.get(guid)?.upgrade()
    }

    /// The size a file row reports: its open state's live length, if any.
    fn stat_of(&self, rec: &EntryRec) -> Stat {
        let size = if rec.kind == KIND_FILE {
            self.live_cell(&rec.guid)
                .map_or(rec.len, |c| c.live_len.load(Ordering::Acquire))
        } else {
            0
        };
        Stat {
            kind: rec.kind,
            size,
            mtime: rec.mtime,
        }
    }

    /// Creates every missing parent directory of `p` and returns the folded
    /// paths it created, outermost first, for [`Self::rollback`] if the
    /// operation that needed them fails. Under `ns`.
    fn ensure_parents(&self, p: &LPath) -> Result<Vec<String>, i32> {
        let mut created = Vec::new();
        for i in 1..p.parts.len() {
            let folded = fold(&p.parts[..i].join("/"));
            let step = match self.get(&folded) {
                Ok(Some(r)) if r.kind == KIND_DIR => Ok(false),
                Ok(Some(_)) => Err(not_a_dir()),
                Ok(None) => self
                    .put(
                        &folded,
                        &EntryRec {
                            name: p.parts[i - 1].clone(),
                            kind: KIND_DIR,
                            guid: [0; 16],
                            len: 0,
                            mtime: now(),
                        },
                    )
                    .map(|()| true),
                Err(e) => Err(e),
            };
            match step {
                Ok(true) => created.push(folded),
                Ok(false) => {}
                Err(e) => {
                    self.rollback(&created);
                    return Err(e);
                }
            }
        }
        Ok(created)
    }

    /// Removes the directory rows [`Self::ensure_parents`] created, innermost
    /// first. Best effort. Under `ns`.
    fn rollback(&self, created: &[String]) {
        for dir in created.iter().rev() {
            let _ = self.storage.catalog.remove(self.id, dir, false);
        }
    }

    /// The shared cell of the file row `rec` at `folded`, created on first
    /// use, with one more open counted. Under `ns`.
    fn acquire(&self, rec: &EntryRec, folded: &str) -> Result<Arc<FileCell>, i32> {
        let mut cells = lock(&self.cells)?;
        let cell = match cells.get(&rec.guid).and_then(Weak::upgrade) {
            Some(c) => c,
            None => {
                let id = layer_file_id(&rec.guid);
                // The store holds the data; after a crash between a store
                // flush and a durable catalog commit the row's length can lag.
                let len = match self.storage.store.stat(&id) {
                    Ok(Some(info)) => {
                        if info.len != rec.len {
                            tracing::warn!(
                                layer = %self.name, path = folded, row = rec.len, store = info.len,
                                "layer file length differs between catalog and store; using the store's"
                            );
                        }
                        info.len
                    }
                    Ok(None) => {
                        tracing::error!(
                            layer = %self.name, path = folded,
                            "layer file missing from the store: corruption"
                        );
                        rec.len
                    }
                    Err(e) => return Err(self.st_err("store stat", e.into())),
                };
                let c = Arc::new(FileCell::new(rec.guid, folded.to_owned(), len));
                cells.retain(|_, w| w.strong_count() > 0);
                cells.insert(rec.guid, Arc::downgrade(&c));
                c
            }
        };
        cell.opens.fetch_add(1, Ordering::AcqRel);
        Ok(cell)
    }

    /// Drops one open of `cell`; the last one of a removed file dooms it.
    /// Returns whether it did (the caller then runs a durable point).
    fn release(&self, cell: &FileCell) -> Result<bool, i32> {
        let _ns = lock(&self.ns)?;
        let last = cell.opens.fetch_sub(1, Ordering::AcqRel) == 1;
        if last && lock(&cell.path)?.is_none() {
            lock(&self.doomed)?.push(cell.guid);
            return Ok(true);
        }
        Ok(false)
    }

    /// The row of `guid` is gone. Under `ns`.
    fn doom(&self, guid: Guid) -> Result<(), i32> {
        self.storage.ram.invalidate_file(&layer_file_id(&guid));
        if let Some(c) = self.live_cell(&guid) {
            *lock(&c.path)? = None;
            if c.opens.load(Ordering::Acquire) > 0 {
                return Ok(()); // its last `release` dooms it
            }
        }
        lock(&self.doomed)?.push(guid);
        Ok(())
    }

    /// Creates the file row at `p` (and any missing parents) and its store
    /// file, and acquires its cell. On failure everything it created is
    /// rolled back. Under `ns`.
    fn create(&self, p: &LPath) -> Result<Arc<FileCell>, i32> {
        let parents = self.ensure_parents(p)?;
        let guid = new_guid();
        let id = layer_file_id(&guid);
        let rec = EntryRec {
            name: p.name().to_owned(),
            kind: KIND_FILE,
            guid,
            len: 0,
            mtime: now(),
        };
        let mut row = false;
        let mut stored = false;
        let made = (|| {
            // Spec §6: the row (non-durable) before the store file.
            self.put(&p.folded, &rec)?;
            row = true;
            #[cfg(test)]
            if self.fail_store_create.swap(false, Ordering::SeqCst) {
                return Err(map_io_err());
            }
            self.storage
                .store
                .set_len(&id, 0)
                .map_err(|e| self.st_err("store create", e.into()))?;
            stored = true;
            self.acquire(&rec, &p.folded)
        })();
        if made.is_err() {
            if row {
                let _ = self.storage.catalog.remove(self.id, &p.folded, false);
            }
            if stored {
                let _ = self.storage.store.delete(&id);
            }
            self.rollback(&parents);
        }
        made
    }

    /// Commits `cell` and, if anything changed, its catalog row's length and
    /// mtime. Called with the cell's state lock held.
    ///
    /// The blocks and the row go in under one shared hold of the durability
    /// gate ([`Storage::gate`]), taken after `state` and before `ns`.
    fn commit(&self, cell: &FileCell, st: &mut FileState) -> Result<(), i32> {
        let _gate = self.storage.gate_shared();
        if !cell.commit(&self.storage, &self.name, st)? {
            return Ok(());
        }
        let _ns = lock(&self.ns)?;
        let Some(path) = lock(&cell.path)?.clone() else {
            return Ok(());
        };
        if let Some(mut rec) = self.get(&path)? {
            if rec.guid == cell.guid {
                rec.len = st.len;
                rec.mtime = lock(&cell.mtime_override)?.unwrap_or_else(now);
                self.put(&path, &rec)?;
            }
        }
        Ok(())
    }

    /// Store flush, then the durable catalog commit, then the store deletes
    /// that commit made safe.
    ///
    /// The flush and the commit run under the exclusive durability gate
    /// ([`Storage::gate`]), so no layer's or cache's row can land between them
    /// ahead of its data. `ns` is held only while the doomed list is taken
    /// (before the fsyncs): every GUID in it had its row removed before the
    /// commit, which therefore makes the removal durable before the store
    /// delete (spec §6). GUIDs doomed later wait for the next durable point.
    pub(crate) fn durable_point(&self) -> Result<(), i32> {
        let s = &self.storage;
        let doomed = {
            let _gate = s.gate_exclusive();
            let doomed = {
                let _ns = lock(&self.ns)?;
                std::mem::take(&mut *lock(&self.doomed)?)
            };
            let flushed = s
                .store
                .flush()
                .map_err(|e| self.st_err("store flush", e.into()))
                .and_then(|()| {
                    s.catalog
                        .commit_durable()
                        .map_err(|e| self.st_err("catalog commit", e))
                });
            if let Err(e) = flushed {
                lock(&self.doomed)?.extend(doomed);
                return Err(e);
            }
            doomed
        };
        for g in doomed {
            let id = layer_file_id(&g);
            s.ram.invalidate_file(&id);
            match s.store.delete(&id) {
                Ok(()) | Err(vfs_block_store::Error::NotFound) => {}
                // Left for reconciliation, which deletes unreferenced ids.
                Err(e) => {
                    tracing::warn!(layer = %self.name, error = %e, "layer file delete failed")
                }
            }
        }
        Ok(())
    }

    fn handle(&self, h: Handle) -> Result<Arc<OpenFile>, i32> {
        lock(&self.handles)?.get(&h).cloned().ok_or_else(bad_fh)
    }

    fn file_of(&self, h: Handle) -> Result<(Arc<OpenFile>, Arc<FileCell>), i32> {
        let of = self.handle(h)?;
        let cell = of.cell.clone().ok_or_else(is_dir)?;
        Ok((of, cell))
    }

    fn track(&self, of: OpenFile) -> Result<Handle, i32> {
        let h = self.next.fetch_add(1, Ordering::Relaxed);
        lock(&self.handles)?.insert(h, Arc::new(of));
        Ok(h)
    }

    /// `set_len` on `cell` with its commit.
    fn resize(&self, cell: &FileCell, len: u64) -> Result<(), i32> {
        let mut st = lock(&cell.state)?;
        cell.truncate(&self.storage, &self.name, &mut st, len)?;
        self.commit(cell, &mut st)
    }
}

impl Provider for LayerProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::ReadWrite,
            immutable: false,
            slow: false,
            preferred_block: Some(self.storage.block_size() as u32),
            case: CaseMatch::Insensitive,
        }
    }

    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        let p = LPath::parse(p.rel)?;
        Ok(self.get(&p.folded)?.map(|r| self.stat_of(&r)))
    }

    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        let p = LPath::parse(p.rel)?;
        match self.get(&p.folded)? {
            None => return Err(not_found()),
            Some(r) if r.kind != KIND_DIR => return Err(not_a_dir()),
            Some(_) => {}
        }
        let rows = self
            .storage
            .catalog
            .children(self.id, &p.folded)
            .map_err(|e| self.st_err("catalog children", e))?;
        Ok(rows
            .into_iter()
            .map(|r| DirEntry {
                stat: self.stat_of(&r),
                name: r.name,
            })
            .collect())
    }

    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        let p = LPath::parse(p.rel)?;
        let create = flags & OPEN_CREATE != 0;
        // A create writes a row, then its store file: a pair under the
        // durability gate, which comes before `ns`.
        let gate = create.then(|| self.storage.gate_shared());
        let ns = lock(&self.ns)?;
        let (cell, created) = match self.get(&p.folded)? {
            Some(r) if r.kind == KIND_DIR => {
                if create && flags & OPEN_EXCL != 0 {
                    return Err(exists());
                }
                if flags & (OPEN_WRITE | OPEN_TRUNC) != 0 {
                    return Err(is_dir());
                }
                drop(ns);
                let h = self.track(OpenFile {
                    cell: None,
                    wrote: AtomicBool::new(false),
                })?;
                return Ok((h, 0, true));
            }
            Some(r) => {
                if flags & OPEN_EXCL != 0 {
                    return Err(exists());
                }
                (self.acquire(&r, &p.folded)?, false)
            }
            None if !create => return Err(not_found()),
            None => (self.create(&p)?, true),
        };
        drop(ns);
        drop(gate);

        let h = self.track(OpenFile {
            cell: Some(Arc::clone(&cell)),
            wrote: AtomicBool::new(created),
        })?;
        if flags & OPEN_TRUNC != 0 && !created {
            if let Err(e) = self.resize(&cell, 0) {
                let _ = self.close(h);
                return Err(e);
            }
            self.handle(h)?.wrote.store(true, Ordering::Release);
        }
        Ok((h, cell.live_len.load(Ordering::Acquire), false))
    }

    fn close(&self, h: Handle) -> Result<(), i32> {
        let of = lock(&self.handles)?.remove(&h).ok_or_else(bad_fh)?;
        let Some(cell) = &of.cell else {
            return Ok(());
        };
        let mut durable = of.wrote.load(Ordering::Acquire);
        let committed = lock(&cell.state).and_then(|mut st| {
            if durable || st.is_dirty() {
                durable = true;
                self.commit(cell, &mut st)
            } else {
                Ok(())
            }
        });
        let doomed = self.release(cell);
        committed?;
        if doomed? || durable {
            self.durable_point()?;
        }
        Ok(())
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (_of, cell) = self.file_of(h)?;
        let st = lock(&cell.state)?;
        cell.read(&self.storage, &self.name, &st, offset, buf)
    }

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let (of, cell) = self.file_of(h)?;
        let mut st = lock(&cell.state)?;
        cell.write(&self.storage, &self.name, &mut st, offset, buf)?;
        of.wrote.store(true, Ordering::Release);
        if st.dirty.len() > 4 {
            self.commit(&cell, &mut st)?;
        }
        Ok(buf.len())
    }

    fn set_len(&self, h: Handle, len: u64) -> Result<(), i32> {
        let (of, cell) = self.file_of(h)?;
        of.wrote.store(true, Ordering::Release);
        self.resize(&cell, len)
    }

    fn flush(&self, h: Handle) -> Result<(), i32> {
        let of = self.handle(h)?;
        let Some(cell) = &of.cell else {
            return Ok(());
        };
        {
            let mut st = lock(&cell.state)?;
            self.commit(cell, &mut st)?;
        }
        self.durable_point()
    }

    fn mkdir(&self, p: VPath) -> Result<(), i32> {
        let p = LPath::parse(p.rel)?;
        if p.is_root() {
            return Ok(());
        }
        {
            let _ns = lock(&self.ns)?;
            match self.get(&p.folded)? {
                Some(r) if r.kind == KIND_DIR => return Ok(()),
                Some(_) => return Err(exists()),
                None => {}
            }
            let parents = self.ensure_parents(&p)?;
            let made = self.put(
                &p.folded,
                &EntryRec {
                    name: p.name().to_owned(),
                    kind: KIND_DIR,
                    guid: [0; 16],
                    len: 0,
                    mtime: now(),
                },
            );
            if made.is_err() {
                self.rollback(&parents);
            }
            made?;
        }
        self.durable_point()
    }

    fn remove(&self, p: VPath) -> Result<(), i32> {
        let p = LPath::parse(p.rel)?;
        if p.is_root() {
            return Err(bad_request());
        }
        {
            let _ns = lock(&self.ns)?;
            let rec = self.get(&p.folded)?.ok_or_else(not_found)?;
            self.storage
                .catalog
                .remove(self.id, &p.folded, false)
                .map_err(|e| self.st_err("catalog remove", e))?;
            if rec.kind == KIND_FILE {
                self.doom(rec.guid)?;
            }
        }
        // Durable now, and the durable point deletes the file's data (unless
        // a handle still has it open).
        self.durable_point()
    }

    fn rename(&self, from: VPath, to: VPath) -> Result<(), i32> {
        if from.root != to.root {
            return Err(bad_request());
        }
        let from = LPath::parse(from.rel)?;
        let to = LPath::parse(to.rel)?;
        if from.is_root() || to.is_root() || to.folded.starts_with(&format!("{}/", from.folded)) {
            return Err(bad_request());
        }
        self.rename_rows(&from, &to)?;
        // A game saves by writing a temp file, closing it and renaming it over
        // the real one: the save is only safe once the rename is durable.
        // The durable point also deletes a replaced file's data.
        self.durable_point()
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        self.set_attr_impl(p, attr)
    }
}

impl LayerProvider {
    /// The namespace half of `rename`, under `ns`.
    fn rename_rows(&self, from: &LPath, to: &LPath) -> Result<(), i32> {
        let _ns = lock(&self.ns)?;
        let from_is_dir = self.get(&from.folded)?.ok_or_else(not_found)?.kind == KIND_DIR;
        let mut parents = Vec::new();
        if from.folded != to.folded {
            let dest = self.get(&to.folded)?;
            // A directory moves only onto a free name: two subtrees cannot be
            // combined (MemoryProvider refuses the same way).
            if from_is_dir && dest.is_some() {
                return Err(exists());
            }
            // Nothing may clobber a directory, empty or not (MemoryProvider,
            // and what Windows' MoveFileEx does).
            if matches!(dest, Some(r) if r.kind == KIND_DIR) {
                return Err(exists());
            }
            parents = self.ensure_parents(to)?;
        }
        let replaced =
            match self
                .storage
                .catalog
                .rename(self.id, &from.folded, &to.folded, to.name())
            {
                Ok(r) => r,
                Err(e) => {
                    self.rollback(&parents);
                    return Err(self.st_err("catalog rename", e));
                }
            };
        for g in replaced {
            self.doom(g)?;
        }
        // Open files under the moved path follow it.
        let prefix = format!("{}/", from.folded);
        for c in lock(&self.cells)?.values().filter_map(Weak::upgrade) {
            let mut path = lock(&c.path)?;
            let moved = match path.as_deref() {
                Some(p) if p == from.folded => Some(to.folded.clone()),
                Some(p) => p
                    .strip_prefix(&prefix)
                    .map(|rest| format!("{}/{rest}", to.folded)),
                None => None,
            };
            if moved.is_some() {
                *path = moved;
            }
        }
        Ok(())
    }

    fn set_attr_impl(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        let p = LPath::parse(p.rel)?;
        if let Some(size) = attr.size {
            let ns = lock(&self.ns)?;
            let rec = self.get(&p.folded)?.ok_or_else(not_found)?;
            if rec.kind != KIND_FILE {
                return Err(is_dir());
            }
            let cell = self.acquire(&rec, &p.folded)?;
            drop(ns);
            let resized = self.resize(&cell, size);
            let doomed = self.release(&cell);
            resized?;
            doomed?;
            self.durable_point()?;
        }
        if let Some(mtime) = attr.mtime {
            let _ns = lock(&self.ns)?;
            let mut rec = self.get(&p.folded)?.ok_or_else(not_found)?;
            rec.mtime = mtime;
            self.put(&p.folded, &rec)?;
            if rec.kind == KIND_FILE {
                if let Some(c) = self.live_cell(&rec.guid) {
                    *lock(&c.mtime_override)? = Some(mtime);
                }
            }
        }
        Ok(())
    }
}

impl Drop for LayerProvider {
    /// Commits whatever handles were left open and runs a last durable
    /// point, which also deletes doomed files; then leaves the storage's
    /// layer registry, so the layer counts as in use until this finishes.
    fn drop(&mut self) {
        /// Leaves the registry even if something below panics, so a later
        /// `Storage::layer(name)` never waits forever for this provider.
        struct Leave<'a>(&'a LayerProvider);
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                self.0.storage.layer_dropped(&self.0.name, self.0);
            }
        }
        let _leave = Leave(self);
        let open: Vec<Arc<FileCell>> = match self.handles.lock() {
            Ok(h) => h.values().filter_map(|of| of.cell.clone()).collect(),
            Err(_) => Vec::new(),
        };
        for cell in open {
            if let Ok(mut st) = cell.state.lock() {
                let _ = self.commit(&cell, &mut st);
            }
        }
        if let Err(e) = self.durable_point() {
            tracing::warn!(layer = %self.name, status = e, "layer close: durable point failed");
        }
        #[cfg(test)]
        {
            let hook = crate::cached::lock(&self.storage.drop_hook).take();
            if let Some(hook) = hook {
                hook();
            }
        }
        // `_leave` drops last: only now may the layer be deleted or get a new
        // provider.
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vfs_provider::{
        Provider, SetAttr, VPath, FIXTURE_FILES, KIND_DIR, KIND_FILE, OPEN_CREATE, OPEN_READ,
        OPEN_TRUNC, OPEN_WRITE, ST_IO_ERROR,
    };

    use crate::config::StorageConfig;
    use crate::ids::layer_file_id;
    use crate::storage::Storage;

    use super::LayerProvider;
    #[cfg(not(windows))]
    use crate::test_util::snapshot;

    const BS: u64 = 4096;

    fn cfg() -> StorageConfig {
        let mut c = StorageConfig::default();
        c.store.block_size = BS as u32;
        c
    }

    fn temp_storage() -> (Arc<Storage>, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        (s, d)
    }

    fn at(p: &str) -> VPath<'_> {
        VPath::at_default(p)
    }

    fn write_file(p: &Arc<dyn Provider>, rel: &str, off: u64, body: &[u8]) {
        let (h, _, _) = p.open(at(rel), OPEN_WRITE | OPEN_CREATE).unwrap();
        assert_eq!(p.write_at(h, off, body).unwrap(), body.len());
        p.close(h).unwrap();
    }

    fn read_file(p: &Arc<dyn Provider>, rel: &str) -> Vec<u8> {
        let (h, size, is_dir) = p.open(at(rel), OPEN_READ).unwrap();
        assert!(!is_dir);
        let out = read_range(p, h, 0, size as usize);
        p.close(h).unwrap();
        out
    }

    fn read_range(p: &Arc<dyn Provider>, h: u64, off: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        let mut done = 0;
        while done < len {
            let n = p.read_at(h, off + done as u64, &mut out[done..]).unwrap();
            if n == 0 {
                break;
            }
            done += n;
        }
        out.truncate(done);
        out
    }

    fn seed_fixture(p: &Arc<dyn Provider>) {
        p.mkdir(at("sub")).unwrap();
        for (rel, body) in FIXTURE_FILES {
            write_file(p, rel, 0, body);
        }
    }

    #[test]
    fn capabilities_are_read_write_insensitive_with_the_block_hint() {
        let (s, _d) = temp_storage();
        let c = s.layer("caps").unwrap().capabilities();
        assert_eq!(c.access, vfs_provider::Access::ReadWrite);
        assert!(!c.immutable && !c.slow);
        assert_eq!(c.preferred_block, Some(BS as u32));
        assert_eq!(c.case, vfs_provider::CaseMatch::Insensitive);
    }

    #[test]
    fn conformance() {
        let (s, _d) = temp_storage();
        let p = s.layer("conf").unwrap();
        seed_fixture(&p);
        vfs_provider::assert_conformance(p);
    }

    #[test]
    fn partial_block_write_preserves_neighbours() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        let mut want = vec![0xAAu8; 3 * BS as usize];
        write_file(&p, "big.bin", 0, &want);

        let (h, _, _) = p.open(at("big.bin"), OPEN_WRITE).unwrap();
        p.write_at(h, BS + 10, b"XYZ").unwrap();
        // Read across the edited block through the writing handle (uncommitted)...
        want[BS as usize + 10..BS as usize + 13].copy_from_slice(b"XYZ");
        assert_eq!(read_range(&p, h, 0, want.len()), want);
        p.close(h).unwrap();
        // ...and after it committed.
        assert_eq!(read_file(&p, "big.bin"), want);
    }

    #[test]
    fn gap_reads_as_zeros() {
        let (s, d) = temp_storage();
        let p = s.layer("l").unwrap();
        let off = 5 * BS + 7;
        let mut want = vec![0u8; off as usize];
        want.extend_from_slice(b"end");

        let (h, _, _) = p.open(at("gap.bin"), OPEN_WRITE | OPEN_CREATE).unwrap();
        p.write_at(h, off, b"end").unwrap();
        assert_eq!(read_range(&p, h, 0, want.len() + 10), want, "before commit");
        p.close(h).unwrap();
        assert_eq!(read_file(&p, "gap.bin"), want, "after close");

        drop(p);
        s.close().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("l").unwrap();
        assert_eq!(read_file(&p, "gap.bin"), want, "after reopen");
    }

    #[test]
    fn truncate_then_grow_zero_fills() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "t.bin", 0, &vec![0xAAu8; 3 * BS as usize]);

        let (h, _, _) = p.open(at("t.bin"), OPEN_WRITE).unwrap();
        p.set_len(h, BS + 5).unwrap();
        p.set_len(h, 3 * BS).unwrap();
        p.close(h).unwrap();

        let got = read_file(&p, "t.bin");
        let mut want = vec![0xAAu8; BS as usize + 5];
        want.resize(3 * BS as usize, 0);
        assert_eq!(got.len(), want.len());
        assert!(got == want, "the tail past bs + 5 must be zeros");
    }

    #[test]
    fn set_len_within_an_uncommitted_extension() {
        // Grow by a write (uncommitted), shrink back into the extension, grow
        // again: the dropped bytes must not come back.
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "x.bin", 0, b"abc");
        let (h, _, _) = p.open(at("x.bin"), OPEN_WRITE).unwrap();
        p.write_at(h, 3, &[0x55; 100]).unwrap();
        p.set_len(h, 10).unwrap();
        p.set_len(h, 50).unwrap();
        p.close(h).unwrap();
        let mut want = b"abc".to_vec();
        want.extend_from_slice(&[0x55; 7]);
        want.resize(50, 0);
        assert_eq!(read_file(&p, "x.bin"), want);
    }

    #[test]
    fn many_dirty_blocks_commit_mid_handle_and_read_back() {
        // More than 4 dirty blocks forces commits while the handle stays open.
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        let body: Vec<u8> = (0..(12 * BS + 99)).map(|i| (i % 251) as u8).collect();
        let (h, _, _) = p.open(at("seq.bin"), OPEN_WRITE | OPEN_CREATE).unwrap();
        for (i, chunk) in body.chunks(1000).enumerate() {
            p.write_at(h, i as u64 * 1000, chunk).unwrap();
        }
        assert_eq!(read_range(&p, h, 0, body.len()), body);
        p.close(h).unwrap();
        assert_eq!(read_file(&p, "seq.bin"), body);
    }

    #[test]
    fn second_handle_sees_uncommitted_writes() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        let (h1, _, _) = p.open(at("shared.txt"), OPEN_WRITE | OPEN_CREATE).unwrap();
        p.write_at(h1, 0, b"0123456789").unwrap();
        let (h2, size, _) = p.open(at("shared.txt"), OPEN_READ).unwrap();
        assert_eq!(size, 10);
        assert_eq!(read_range(&p, h2, 0, 10), b"0123456789");
        assert_eq!(p.getattr(at("shared.txt")).unwrap().unwrap().size, 10);
        p.close(h2).unwrap();
        p.close(h1).unwrap();
    }

    #[test]
    fn rename_moves_without_copying() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        p.mkdir(at("dir")).unwrap();
        let body: Vec<u8> = (0..2 * BS).map(|i| (i % 7) as u8).collect();
        write_file(&p, "dir/f.bin", 0, &body);

        let before = s.store.stats().unwrap();
        p.rename(at("dir"), at("moved")).unwrap();
        let after = s.store.stats().unwrap();
        assert_eq!(before.unflushed_bytes, after.unflushed_bytes);
        assert_eq!(before.unflushed_commits, after.unflushed_commits);
        let live = |st: &vfs_block_store::Stats| st.packs.iter().map(|p| p.live_bytes).sum::<u64>();
        assert_eq!(live(&before), live(&after));

        assert!(p.getattr(at("dir/f.bin")).unwrap().is_none());
        assert_eq!(read_file(&p, "moved/f.bin"), body);
    }

    #[test]
    fn open_handle_survives_rename_of_its_directory() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        let (h, _, _) = p.open(at("d/f.txt"), OPEN_WRITE | OPEN_CREATE).unwrap();
        p.write_at(h, 0, b"one").unwrap();
        p.rename(at("d"), at("e")).unwrap();
        p.write_at(h, 3, b"two").unwrap();
        p.close(h).unwrap();
        assert_eq!(read_file(&p, "e/f.txt"), b"onetwo");
        assert_eq!(p.getattr(at("e/f.txt")).unwrap().unwrap().size, 6);
        assert!(p.getattr(at("d/f.txt")).unwrap().is_none());
        assert!(p.getattr(at("d")).unwrap().is_none());
    }

    /// Opens a kill-time copy of `d`'s storage and its layer `name`.
    #[cfg(not(windows))]
    fn killed_copy(
        d: &std::path::Path,
        name: &str,
    ) -> (Arc<Storage>, Arc<dyn Provider>, tempfile::TempDir) {
        let killed = tempfile::tempdir().unwrap();
        snapshot(d, killed.path());
        let k = Storage::open(killed.path(), cfg()).unwrap();
        let kp = k.layer(name).unwrap();
        (k, kp, killed)
    }

    #[cfg(not(windows))]
    fn names(p: &Arc<dyn Provider>, dir: &str) -> Vec<String> {
        p.readdir(at(dir))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    /// The reviewer's reproduction: save to a temp file, rename it over the
    /// real save, get killed. The rename must have been durable.
    #[cfg(not(windows))]
    #[test]
    fn save_then_rename_over_is_durable() {
        let (s, d) = temp_storage();
        let p = s.layer("saves").unwrap();
        write_file(&p, "save.ess", 0, b"old save");
        write_file(&p, "save.tmp", 0, b"new save");
        p.rename(at("save.tmp"), at("save.ess")).unwrap();
        let (_k, kp, _kd) = killed_copy(d.path(), "saves");
        assert_eq!(names(&kp, ""), ["save.ess"]);
        assert_eq!(read_file(&kp, "save.ess"), b"new save");
        drop(p);
    }

    #[cfg(not(windows))]
    #[test]
    fn save_then_rename_to_a_fresh_name_is_durable() {
        let (s, d) = temp_storage();
        let p = s.layer("saves").unwrap();
        write_file(&p, "Saves/save5.tmp", 0, b"fifth");
        p.rename(at("Saves/save5.tmp"), at("Saves/save5.ess"))
            .unwrap();
        let (_k, kp, _kd) = killed_copy(d.path(), "saves");
        assert_eq!(names(&kp, "saves"), ["save5.ess"]);
        assert_eq!(read_file(&kp, "saves/save5.ess"), b"fifth");
    }

    #[cfg(not(windows))]
    #[test]
    fn remove_and_mkdir_are_durable() {
        let (s, d) = temp_storage();
        let p = s.layer("saves").unwrap();
        write_file(&p, "a.ess", 0, b"a");
        write_file(&p, "b.ess", 0, b"b");
        p.remove(at("a.ess")).unwrap();
        p.mkdir(at("Backups/Old")).unwrap();
        let (_k, kp, _kd) = killed_copy(d.path(), "saves");
        assert_eq!(names(&kp, ""), ["b.ess", "Backups"]); // folded key order
        assert_eq!(names(&kp, "backups"), ["Old"]);
        assert_eq!(read_file(&kp, "b.ess"), b"b");
    }

    #[test]
    fn remove_deletes_the_store_file_at_once_when_closed() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "x", 0, b"x");
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        let id = layer_file_id(&s.catalog.get(lid, "x").unwrap().unwrap().guid);
        p.remove(at("x")).unwrap();
        assert!(s.store.stat(&id).unwrap().is_none());
    }

    /// A shrink whose commit fails must still drop the cut bytes: a later
    /// grow (by `set_len` or a write) reads zeros there, before and after it
    /// commits, never the store's stale copy.
    #[test]
    fn a_failed_shrink_commit_never_resurrects_the_cut_bytes() {
        for (cut, regrow) in [(BS + 5, "set_len"), (BS, "set_len"), (BS + 5, "write")] {
            let (s, _d) = temp_storage();
            let lid = s.catalog.create_layer("l").unwrap();
            let lp: Arc<LayerProvider> =
                Arc::new(LayerProvider::new(Arc::clone(&s), "l".into(), lid));
            let p: Arc<dyn Provider> = lp.clone();
            write_file(&p, "f", 0, &vec![0xAAu8; 3 * BS as usize]);
            let guid = s.catalog.get(lid, "f").unwrap().unwrap().guid;

            let (h, _, _) = p.open(at("f"), OPEN_WRITE).unwrap();
            let cell = lp.live_cell(&guid).unwrap();
            cell.fail_commit
                .store(true, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(p.set_len(h, cut), Err(ST_IO_ERROR), "{cut} {regrow}");
            let mut want = vec![0xAAu8; cut as usize];
            want.resize(3 * BS as usize, 0);
            if regrow == "set_len" {
                p.set_len(h, 3 * BS).unwrap();
            } else {
                p.write_at(h, 3 * BS - 1, &[0]).unwrap();
            }
            assert!(
                read_range(&p, h, 0, want.len()) == want,
                "{cut} {regrow}: open handle"
            );
            p.close(h).unwrap();
            assert!(read_file(&p, "f") == want, "{cut} {regrow}: after close");
        }
    }

    #[test]
    fn a_failed_create_rolls_back_its_parent_directories() {
        let (s, _d) = temp_storage();
        let id = s.catalog.create_layer("l").unwrap();
        let lp = LayerProvider::new(Arc::clone(&s), "l".into(), id);
        lp.mkdir(at("keep")).unwrap();
        lp.fail_store_create
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            lp.open(at("keep/new/deeper/f.txt"), OPEN_WRITE | OPEN_CREATE)
                .map(|_| ()),
            Err(ST_IO_ERROR)
        );
        assert!(lp.getattr(at("keep/new")).unwrap().is_none());
        assert!(lp.getattr(at("keep/new/deeper/f.txt")).unwrap().is_none());
        assert!(lp.getattr(at("keep")).unwrap().is_some());
    }

    #[test]
    fn closed_file_is_durable_across_storage_reopen() {
        let (s, d) = temp_storage();
        let p = s.layer("saves").unwrap();
        write_file(&p, "Saves/one.ess", 0, b"saved game");
        // A second file still open (unflushed) must not stop the first from
        // being durable.
        let (h, _, _) = p
            .open(at("Saves/two.ess"), OPEN_WRITE | OPEN_CREATE)
            .unwrap();
        p.write_at(h, 0, b"in progress").unwrap();

        // Killed now: nothing but the close of one.ess has flushed anything.
        // Not on Windows, where redb and the store hold mandatory locks that
        // make a live copy fail.
        #[cfg(not(windows))]
        {
            let killed = tempfile::tempdir().unwrap();
            snapshot(d.path(), killed.path());
            let k = Storage::open(killed.path(), cfg()).unwrap();
            let kp = k.layer("saves").unwrap();
            assert_eq!(read_file(&kp, "saves/ONE.ess"), b"saved game");
        }

        // And the brief's variant: drop Storage without `close()`.
        p.close(h).unwrap();
        drop(p);
        drop(s);
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("saves").unwrap();
        assert_eq!(read_file(&p, "saves/one.ess"), b"saved game");
        assert_eq!(read_file(&p, "saves/two.ess"), b"in progress");
    }

    #[test]
    fn missing_committed_block_is_an_io_error() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "f.bin", 0, &vec![1u8; 2 * BS as usize]);
        let rec = s
            .catalog
            .get(s.catalog.layer_id("l").unwrap().unwrap(), "f.bin")
            .unwrap()
            .unwrap();
        s.ram.invalidate_file(&layer_file_id(&rec.guid));
        s.store.delete(&layer_file_id(&rec.guid)).unwrap();

        let (h, _, _) = p.open(at("f.bin"), OPEN_READ).unwrap();
        assert_eq!(p.read_at(h, 0, &mut [0u8; 16]), Err(ST_IO_ERROR));
        p.close(h).unwrap();
    }

    #[test]
    fn case_insensitive_lookup_preserves_case() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "Saves/One.ESS", 0, b"x");
        let st = p.getattr(at("saves/one.ess")).unwrap().unwrap();
        assert_eq!((st.kind, st.size), (KIND_FILE, 1));
        let names: Vec<String> = p
            .readdir(at("SAVES"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["One.ESS"]);
        let root: Vec<(String, u8)> = p
            .readdir(at(""))
            .unwrap()
            .into_iter()
            .map(|e| (e.name, e.stat.kind))
            .collect();
        assert_eq!(root, [("Saves".to_string(), KIND_DIR)]);
    }

    #[test]
    fn a_read_fill_before_a_write_never_serves_the_old_block() {
        // Rule 4 (the RAM tier's stale-refill race): a read that fills the RAM
        // tier with a block, then a write and commit of that block, then a read.
        // Every fill and every commit of a layer file happens under the file's
        // state lock, so the only order the race can take is this one; the read
        // after the commit must see the new bytes, from RAM or the store.
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "r.bin", 0, &vec![1u8; 2 * BS as usize]);
        let (h, _, _) = p.open(at("r.bin"), OPEN_WRITE).unwrap();
        assert_eq!(read_range(&p, h, 0, 4), [1, 1, 1, 1]); // fills block 0 into RAM
        let hits = s.ram.stats().hits;
        p.write_at(h, 0, &[2u8; 4]).unwrap();
        p.flush(h).unwrap();
        let (r, _, _) = p.open(at("r.bin"), OPEN_READ).unwrap();
        assert_eq!(read_range(&p, r, 0, 5), [2, 2, 2, 2, 1]);
        assert!(
            s.ram.stats().hits > hits,
            "the committed block is served from RAM"
        );
        p.close(r).unwrap();
        p.close(h).unwrap();
    }

    #[test]
    fn remove_while_open_defers_the_store_delete_to_last_close() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "gone.txt", 0, b"still readable");
        let rec = s
            .catalog
            .get(s.catalog.layer_id("l").unwrap().unwrap(), "gone.txt")
            .unwrap()
            .unwrap();
        let id = layer_file_id(&rec.guid);
        let (h, _, _) = p.open(at("gone.txt"), OPEN_READ).unwrap();
        p.remove(at("gone.txt")).unwrap();
        assert!(p.getattr(at("gone.txt")).unwrap().is_none());
        assert_eq!(read_range(&p, h, 0, 14), b"still readable");
        assert!(s.store.stat(&id).unwrap().is_some());
        p.close(h).unwrap();
        assert!(
            s.store.stat(&id).unwrap().is_none(),
            "last close deletes it"
        );
    }

    #[test]
    fn rename_over_a_file_deletes_the_replaced_data() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "a.txt", 0, b"new");
        write_file(&p, "b.txt", 0, b"old");
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        let old = layer_file_id(&s.catalog.get(lid, "b.txt").unwrap().unwrap().guid);
        p.rename(at("a.txt"), at("B.TXT")).unwrap();
        assert_eq!(read_file(&p, "b.txt"), b"new");
        let names: Vec<String> = p
            .readdir(at(""))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["B.TXT"]);
        // Replaced data goes at the next durable point.
        let (h, _, _) = p.open(at("b.txt"), OPEN_WRITE).unwrap();
        p.flush(h).unwrap();
        p.close(h).unwrap();
        assert!(s.store.stat(&old).unwrap().is_none());
    }

    #[test]
    fn directory_rules() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "d/f.txt", 0, b"x");
        assert_eq!(p.remove(at("d")), Err(vfs_provider::ST_IS_DIR));
        assert_eq!(
            p.rename(at("d"), at("d/inner")),
            Err(vfs_provider::ST_BAD_REQUEST)
        );
        p.mkdir(at("e")).unwrap();
        assert_eq!(
            p.rename(at("d/f.txt"), at("e")),
            Err(vfs_provider::ST_EXISTS)
        );
        assert_eq!(
            p.open(at("d"), OPEN_WRITE).map(|_| ()),
            Err(vfs_provider::ST_IS_DIR)
        );
        assert_eq!(
            p.open(at("d/f.txt/x"), OPEN_WRITE | OPEN_CREATE)
                .map(|_| ()),
            Err(vfs_provider::ST_NOT_A_DIRECTORY)
        );
        assert_eq!(
            p.readdir(at("d/f.txt")),
            Err(vfs_provider::ST_NOT_A_DIRECTORY)
        );
        assert_eq!(p.readdir(at("nope")), Err(vfs_provider::ST_NOT_FOUND));
        p.mkdir(at("e"))
            .expect("mkdir of an existing directory is idempotent");
        assert_eq!(p.mkdir(at("d/f.txt")), Err(vfs_provider::ST_EXISTS));
        assert_eq!(p.remove(at("nope")), Err(vfs_provider::ST_NOT_FOUND));
        let (h, size, is_dir) = p.open(at("D"), OPEN_READ).unwrap();
        assert_eq!((size, is_dir), (0, true));
        p.close(h).unwrap();
    }

    #[test]
    fn absurd_lengths_are_refused_without_poisoning_the_file() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        let (h, _, _) = p.open(at("f"), OPEN_WRITE | OPEN_CREATE).unwrap();
        assert_eq!(
            p.write_at(h, u64::MAX - 1, b"xy"),
            Err(vfs_provider::ST_BAD_REQUEST)
        );
        assert_eq!(p.write_at(h, 1 << 60, b"x"), Err(vfs_provider::ST_NO_SPACE));
        assert_eq!(p.set_len(h, 1 << 60), Err(vfs_provider::ST_NO_SPACE));
        p.write_at(h, 0, b"ok").unwrap();
        p.close(h).unwrap();
        assert_eq!(read_file(&p, "f"), b"ok");
    }

    #[test]
    fn set_attr_size_and_mtime() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "f.txt", 0, b"hello world");
        p.set_attr(
            at("F.txt"),
            SetAttr {
                size: Some(5),
                mtime: Some(1_700_000_000),
            },
        )
        .unwrap();
        let st = p.getattr(at("f.txt")).unwrap().unwrap();
        assert_eq!((st.size, st.mtime), (5, 1_700_000_000));
        assert_eq!(read_file(&p, "f.txt"), b"hello");
        assert_eq!(
            p.set_attr(
                at("nope"),
                SetAttr {
                    size: Some(1),
                    mtime: None
                }
            ),
            Err(vfs_provider::ST_NOT_FOUND)
        );
    }

    #[test]
    fn truncate_on_open() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "f.txt", 0, &vec![3u8; BS as usize + 1]);
        let (h, size, _) = p.open(at("f.txt"), OPEN_WRITE | OPEN_TRUNC).unwrap();
        assert_eq!(size, 0);
        p.close(h).unwrap();
        assert_eq!(read_file(&p, "f.txt"), b"");
    }

    #[test]
    fn the_same_layer_is_one_provider_and_in_use_while_alive() {
        let (s, _d) = temp_storage();
        let a = s.layer("one").unwrap();
        let b = s.layer("one").unwrap();
        let _c = s.layer("two").unwrap();
        // One namespace: a write through one is visible through the other,
        // uncommitted.
        let (h, _, _) = a.open(at("f"), OPEN_WRITE | OPEN_CREATE).unwrap();
        a.write_at(h, 0, b"abc").unwrap();
        assert_eq!(b.getattr(at("f")).unwrap().unwrap().size, 3);
        a.close(h).unwrap();
        assert_eq!(s.layers_in_use(), ["one", "two"]);
        drop(a);
        drop(b);
        assert_eq!(s.layers_in_use(), ["two"]);
        // Reopening finds the same layer.
        let a = s.layer("one").unwrap();
        assert_eq!(read_file(&a, "f"), b"abc");
    }

    #[test]
    fn overlay_copy_up_into_a_layer() {
        let (s, _d) = temp_storage();
        let upper = s.layer("upper").unwrap();
        let base: Arc<dyn Provider> = Arc::new(vfs_provider::conformance::MemFixture::new());
        let ov: Arc<dyn Provider> = Arc::new(
            vfs_compose::OverlayProvider::from_arcs(Arc::clone(&base), Arc::clone(&upper)).unwrap(),
        );

        // Root file, and one under a directory only the base has: the upper has
        // no `sub`, so the copy-up must be able to create it.
        for (rel, body) in [("a.txt", &b"hello"[..]), ("sub/b.txt", &b"world!"[..])] {
            let (h, _, _) = ov.open(at(rel), OPEN_WRITE).unwrap();
            ov.write_at(h, 0, b"J").unwrap();
            ov.close(h).unwrap();
            let mut want = body.to_vec();
            want[0] = b'J';
            assert_eq!(read_file(&ov, rel), want, "{rel} through the overlay");
            assert_eq!(read_file(&upper, rel), want, "{rel} in the layer");
            assert_eq!(
                read_file(&base, rel),
                body,
                "{rel} in the base is untouched"
            );
        }

        // A removal is a whiteout in the layer.
        ov.remove(at("a.txt")).unwrap();
        assert!(ov.getattr(at("a.txt")).unwrap().is_none());
        assert!(upper.getattr(at(".wh.a.txt")).unwrap().is_some());
    }

    #[test]
    fn an_overlay_over_a_layer_passes_conformance() {
        let (s, _d) = temp_storage();
        let upper = s.layer("upper").unwrap();
        let base: Arc<dyn Provider> = Arc::new(vfs_provider::conformance::MemFixture::new());
        let ov = vfs_compose::OverlayProvider::from_arcs(base, upper).unwrap();
        vfs_provider::assert_conformance(Arc::new(ov));
    }
}
