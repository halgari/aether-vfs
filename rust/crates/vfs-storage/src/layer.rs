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
//! **Durability (spec §6).** A *durable point* is `BlockStore::flush()`, then
//! the catalog's durable commit, under the exclusive durability gate
//! ([`Storage::gate`]), so no file's row update can slip into the durable
//! catalog commit without its blocks being in the store flush. Every other
//! catalog write is non-durable. When durable points happen is
//! [`crate::Durability`]'s choice:
//!
//! - under `OnEveryClose`, a handle's `flush`, the `close` of a handle that
//!   wrote, and every namespace change (`mkdir`, `remove`, `rename`, a size
//!   change by `set_attr`) commit and then run one before returning;
//! - under `Deferred { max_interval }` (the default), the same operations
//!   commit exactly as above but skip the fsyncs, unless a durable point is
//!   due (the last is `max_interval` old, or the catalog holds
//!   [`crate::storage::DEFERRED_MAX_COMMITS`] non-durable commits) — with one
//!   exception: the `close`, `flush` or `set_attr` size change of a file that
//!   wrote to a file whose row is already durable (it existed at the last
//!   durable point) runs one at once. Rewriting such a file in place changes
//!   store data a durable row describes; left non-durable, a store
//!   auto-flush in the middle of the rewrite could publish a store state the
//!   durable row does not match, and a crash would leave the file emptied or
//!   torn. Files created since the last durable point (tracked per layer in
//!   `fresh`) stay deferred, and so do namespace changes;
//! - under both, [`Storage::sync`], [`Storage::close`] and a provider's `Drop`
//!   always run one (skipping the fsyncs when nothing is non-durable), as do
//!   layer creation, import and deletion.
//!
//! A file whose row is removed or replaced is deleted from the store only
//! after a durable point has made the row's removal durable (catalog first,
//! store second), and only once no handle has it open: until then its GUID
//! waits in the storage's `doomed` list, for as long as the policy defers.
//! A crash therefore loses at most the changes since the last durable point,
//! and reconciliation at the next open repairs the store to match
//! (see [`crate::Durability`] for what a deferred crash leaves).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};

use vfs_core::fold;
use vfs_provider::{
    bad_fh, bad_request, exists, is_dir, map_io_err, not_a_dir, not_found, Access, Capabilities,
    CaseMatch, DirEntry, Handle, Provider, SetAttr, Stat, VPath, KIND_DIR, KIND_FILE, OPEN_CREATE,
    OPEN_EXCL, OPEN_TRUNC, OPEN_WRITE,
};

use crate::catalog::EntryRec;
use crate::config::Durability;
use crate::ids::{layer_file_id, new_guid, Guid};
use crate::layer_io::{FileCell, FileState};
use crate::storage::{Storage, StorageError};

fn lock<T>(m: &Mutex<T>) -> Result<MutexGuard<'_, T>, i32> {
    m.lock().map_err(|_| map_io_err())
}

/// A file cell's state, shared: a read (see `layer_io`'s locking notes).
fn read_state(m: &RwLock<FileState>) -> Result<RwLockReadGuard<'_, FileState>, i32> {
    m.read().map_err(|_| map_io_err())
}

/// A file cell's state, exclusive: a write, truncate or commit.
fn write_state(m: &RwLock<FileState>) -> Result<RwLockWriteGuard<'_, FileState>, i32> {
    m.write().map_err(|_| map_io_err())
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

/// The folded path [`LPath::parse`] gives for `rel`, without the components:
/// what a lookup that creates nothing needs. `getattr` runs for every
/// metadata question the overlay above passes down — nearly always for a
/// path this layer does not hold — so it is kept to two allocations.
fn folded_path(rel: &str) -> Result<String, i32> {
    let mut joined = String::with_capacity(rel.len());
    for c in rel.split(['/', '\\']).filter(|c| !c.is_empty()) {
        if c == "." || c == ".." {
            return Err(bad_request());
        }
        if !joined.is_empty() {
            joined.push('/');
        }
        joined.push_str(c);
    }
    Ok(fold(&joined))
}

/// One open handle.
struct OpenFile {
    /// `None` for a directory handle.
    cell: Option<Arc<FileCell>>,
    /// The handle created, truncated, wrote or resized its file: its close
    /// is a durable point (or, deferred, a change that may run one).
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
    /// Files created since the last durable point: the durability epoch
    /// ([`crate::storage::DurableClock::epoch`]) their creates saw, and their
    /// GUIDs. A set whose epoch is not the current one is stale (a durable
    /// point has published those rows since) and counts as empty.
    fresh: Mutex<(u64, HashSet<Guid>)>,
    /// Test hook: the next file create fails at the store.
    #[cfg(test)]
    pub(crate) fail_store_create: AtomicBool,
    /// Test hook: the next `put_files` fails right after its rows are
    /// committed (as a poisoned lock while dooming replaced files would).
    #[cfg(test)]
    pub(crate) fail_after_rows: AtomicBool,
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
            fresh: Mutex::new((0, HashSet::new())),
            #[cfg(test)]
            fail_store_create: AtomicBool::new(false),
            #[cfg(test)]
            fail_after_rows: AtomicBool::new(false),
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
            lock(&self.storage.doomed)?.push(cell.guid);
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
        lock(&self.storage.doomed)?.push(guid);
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
            self.created_fresh(guid)?;
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
    ///
    /// A commit that fails after it changed the store's length (a large
    /// shrink stops at a block boundary, see [`FileCell::commit`]) still sets
    /// the row's length to the store's, so `getattr` and a later `open` agree.
    fn commit(&self, cell: &FileCell, st: &mut FileState) -> Result<(), i32> {
        let _gate = self.storage.gate_shared();
        let before = st.committed_len;
        match cell.commit(&self.storage, &self.name, st) {
            Ok(true) => self.update_row(cell, st.len, true),
            Ok(false) => Ok(()),
            Err(e) => {
                if st.committed_len != before {
                    let _ = self.update_row(cell, st.committed_len, false);
                }
                Err(e)
            }
        }
    }

    /// Sets `cell`'s row length to `len` (and its mtime, if `stamp`), if the
    /// row is still `cell`'s. Under the gate, which the caller holds.
    fn update_row(&self, cell: &FileCell, len: u64, stamp: bool) -> Result<(), i32> {
        let _ns = lock(&self.ns)?;
        let Some(path) = lock(&cell.path)?.clone() else {
            return Ok(());
        };
        if let Some(mut rec) = self.get(&path)? {
            if rec.guid == cell.guid {
                rec.len = len;
                if stamp {
                    rec.mtime = lock(&cell.mtime_override)?.unwrap_or_else(now);
                }
                self.put(&path, &rec)?;
            }
        }
        Ok(())
    }

    /// A durable point ([`Storage::durable_point`]).
    pub(crate) fn durable_point(&self) -> Result<(), i32> {
        self.storage
            .durable_point()
            .map_err(|e| self.st_err("durable point", e))
    }

    /// Records that the file `guid` was created in the current durability
    /// epoch. Under the shared gate (a create's), so no durable point runs
    /// between the row's put and this.
    fn created_fresh(&self, guid: Guid) -> Result<(), i32> {
        let epoch = self.storage.clock.epoch();
        let mut fresh = lock(&self.fresh)?;
        if fresh.0 != epoch {
            *fresh = (epoch, HashSet::new());
        }
        fresh.1.insert(guid);
        Ok(())
    }

    /// Whether no durable point has published the row of `guid` since its
    /// create (so its whole content is still non-durable). A race with a
    /// durable point answers false, which only costs an extra one.
    fn is_fresh(&self, guid: &Guid) -> bool {
        let epoch = self.storage.clock.epoch();
        self.fresh
            .lock()
            .is_ok_and(|f| f.0 == epoch && f.1.contains(guid))
    }

    /// Whether `cell`'s file is in one of the storage's scratch directories
    /// ([`crate::StorageConfig::scratch_dirs`]): a temporary its host
    /// deletes after a crash, so a rewrite of it never needs a durable point.
    fn is_scratch(&self, cell: &FileCell) -> bool {
        let dirs = &self.storage.cfg.scratch_dirs;
        if dirs.is_empty() {
            return false;
        }
        let path = cell.path.lock().unwrap_or_else(|e| e.into_inner());
        path.as_deref()
            .and_then(|p| p.split_once('/'))
            .is_some_and(|(top, _)| dirs.iter().any(|d| fold(d) == top))
    }

    /// Called after a change that [`Durability::OnEveryClose`] makes durable
    /// before it returns: under that policy, a [`Self::durable_point`].
    /// Under [`Durability::Deferred`] the change stays non-durable (and a
    /// removed file's store data stays, doomed) unless a durable point is
    /// due, or `rewrote` names a file whose row is already durable (see the
    /// module docs): then this runs one. Called with no lock held.
    fn changed(&self, rewrote: Option<&FileCell>) -> Result<(), i32> {
        let max_interval = match self.storage.durability() {
            Durability::OnEveryClose => return self.durable_point(),
            Durability::Deferred { max_interval } => max_interval,
        };
        if rewrote.is_some_and(|c| !self.is_fresh(&c.guid) && !self.is_scratch(c)) {
            return self.durable_point();
        }
        if !self.storage.deferred_point_due(max_interval) {
            return Ok(());
        }
        self.durable_point()
            .inspect_err(|_| self.storage.clock.retry())
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
        let mut st = write_state(&cell.state)?;
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
        let folded = folded_path(p.rel)?;
        Ok(self.get(&folded)?.map(|r| self.stat_of(&r)))
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
        let committed = write_state(&cell.state).and_then(|mut st| {
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
            self.changed(durable.then_some(&**cell))?;
        }
        Ok(())
    }

    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let (_of, cell) = self.file_of(h)?;
        // Shared: reads of one file run concurrently. Every change to the
        // state holds it exclusive, so this read sees it whole.
        let st = read_state(&cell.state)?;
        cell.read(&self.storage, &self.name, &st, offset, buf)
    }

    fn write_at(&self, h: Handle, offset: u64, buf: &[u8]) -> Result<usize, i32> {
        let (of, cell) = self.file_of(h)?;
        let mut st = write_state(&cell.state)?;
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
        let wrote = {
            let mut st = write_state(&cell.state)?;
            let wrote = of.wrote.load(Ordering::Acquire) || st.is_dirty();
            self.commit(cell, &mut st)?;
            wrote
        };
        self.changed(wrote.then_some(&**cell))
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
        self.changed(None)
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
        // Durable now (or, deferred, at the next durable point), and the
        // durable point deletes the file's data (unless a handle still has it
        // open).
        self.changed(None)
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
        // the real one: the save is only safe once the rename is durable
        // (under `OnEveryClose` at once; deferred, at the next durable
        // point). The durable point also deletes a replaced file's data.
        self.changed(None)
    }

    fn set_attr(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        self.set_attr_impl(p, attr)
    }

    /// One catalog row: the name is stored beside the folded key.
    fn stored_name(&self, p: VPath) -> Result<Option<String>, i32> {
        let folded = folded_path(p.rel)?;
        if folded.is_empty() {
            return Ok(None);
        }
        Ok(self.get(&folded)?.map(|r| r.name))
    }
}

impl LayerProvider {
    /// Creates (or replaces) every file of `files` (path, whole content):
    /// all their data in one block-store commit, then all their rows in one
    /// catalog commit, instead of the five commits a create, a write, a
    /// close and a rename over the real name cost each. Each file appears
    /// whole or not at all: its row is written after its data, as a
    /// temporary file renamed over the real one would be, and a crash
    /// before the next durable point loses the batch whole. Missing parent
    /// directories are created; a path that is a directory, or listed
    /// twice, fails the batch before anything is written.
    pub fn put_files(&self, files: &[(&str, &[u8])]) -> Result<(), i32> {
        let paths: Vec<LPath> = files
            .iter()
            .map(|(p, _)| LPath::parse(p))
            .collect::<Result<_, _>>()?;
        let mut seen = HashSet::new();
        if paths
            .iter()
            .any(|p| p.is_root() || !seen.insert(p.folded.clone()))
        {
            return Err(bad_request());
        }
        let guids: Vec<Guid> = files.iter().map(|_| new_guid()).collect();
        let ids: Vec<[u8; 17]> = guids.iter().map(layer_file_id).collect();
        {
            // Data, then rows, under one shared hold of the gate (as a
            // layer commit): no durable point lands between them.
            let _gate = self.storage.gate_shared();
            {
                let _ns = lock(&self.ns)?;
                for p in &paths {
                    if matches!(self.get(&p.folded)?, Some(r) if r.kind == KIND_DIR) {
                        return Err(is_dir());
                    }
                }
            }
            let batch: Vec<(&[u8], &[u8])> = ids
                .iter()
                .zip(files)
                .map(|(id, (_, data))| (id.as_slice(), *data))
                .collect();
            self.storage
                .store
                .put_files(&batch)
                .map_err(|e| self.st_err("store put", e.into()))?;
            // Set once the rows are committed: from then on the data is
            // theirs, whatever fails after.
            let mut committed = false;
            let rows = (|| {
                let _ns = lock(&self.ns)?;
                let mut rows = Vec::with_capacity(files.len());
                let mut replaced = Vec::new();
                for ((p, guid), (_, data)) in paths.iter().zip(&guids).zip(files) {
                    self.ensure_parents(p)?;
                    match self.get(&p.folded)? {
                        Some(r) if r.kind == KIND_DIR => return Err(is_dir()),
                        Some(r) => replaced.push(r.guid),
                        None => {}
                    }
                    rows.push((
                        p.folded.clone(),
                        EntryRec {
                            name: p.name().to_owned(),
                            kind: KIND_FILE,
                            guid: *guid,
                            len: data.len() as u64,
                            mtime: now(),
                        },
                    ));
                }
                self.storage
                    .catalog
                    .put_many(self.id, &rows, false)
                    .map_err(|e| self.st_err("catalog put", e))?;
                committed = true;
                #[cfg(test)]
                if self.fail_after_rows.swap(false, Ordering::SeqCst) {
                    return Err(vfs_provider::ST_IO_ERROR);
                }
                for g in replaced {
                    self.doom(g)?;
                }
                for g in &guids {
                    self.created_fresh(*g)?;
                }
                Ok(())
            })();
            if let Err(e) = rows {
                if !committed {
                    // No row names them: their data goes now (or, if this
                    // fails too, at the next open's reconciliation).
                    for id in &ids {
                        let _ = self.storage.store.delete(id);
                    }
                }
                return Err(e);
            }
        }
        self.changed(None)
    }

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
            self.changed(Some(&cell))?;
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
            if let Ok(mut st) = cell.state.write() {
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
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use vfs_provider::{
        Provider, SetAttr, VPath, FIXTURE_FILES, KIND_DIR, KIND_FILE, OPEN_CREATE, OPEN_READ,
        OPEN_TRUNC, OPEN_WRITE, ST_IO_ERROR,
    };

    use crate::config::{Durability, StorageConfig};
    use crate::ids::layer_file_id;
    use crate::storage::Storage;

    use super::{folded_path, LPath, LayerProvider};
    #[cfg(not(windows))]
    use crate::test_util::snapshot;

    const BS: u64 = 4096;

    fn cfg() -> StorageConfig {
        let mut c = StorageConfig::default();
        c.store.block_size = BS as u32;
        c
    }

    fn temp_storage() -> (Arc<Storage>, tempfile::TempDir) {
        temp_storage_with(Durability::default())
    }

    /// A storage whose every close, flush and namespace change is a durable
    /// point: what tests of those points' guarantees run on.
    fn temp_storage_every_close() -> (Arc<Storage>, tempfile::TempDir) {
        temp_storage_with(Durability::OnEveryClose)
    }

    fn temp_storage_with(durability: Durability) -> (Arc<Storage>, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(
            d.path(),
            StorageConfig {
                durability,
                ..cfg()
            },
        )
        .unwrap();
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
    fn put_files_writes_a_batch_whole_and_replaces_files() {
        let (s, d) = temp_storage();
        let p = s.layer("batch").unwrap();
        write_file(&p, "c/old", 0, b"the old bytes, long gone");
        let big: Vec<u8> = (0..3 * BS as usize + 5).map(|i| (i % 251) as u8).collect();
        s.put_files(
            "batch",
            &[
                ("c/old", b"new"),
                ("c/Big", &big),
                ("deep/er/x", b"x"),
                ("c/empty", b""),
            ],
        )
        .unwrap();
        assert_eq!(read_file(&p, "c/old"), b"new");
        assert_eq!(read_file(&p, "c/big"), big);
        assert_eq!(read_file(&p, "deep/er/x"), b"x");
        assert_eq!(read_file(&p, "c/empty"), b"");
        let st = p.getattr(at("c/Big")).unwrap().unwrap();
        assert_eq!((st.kind, st.size), (KIND_FILE, big.len() as u64));
        assert_eq!(
            p.stored_name(at("c/big")).unwrap().as_deref(),
            Some("Big"),
            "the name as given"
        );
        // A directory in the way, or a path twice: nothing is written.
        assert!(s
            .put_files("batch", &[("c/new", b"1"), ("deep", b"2")])
            .is_err());
        assert!(s
            .put_files("batch", &[("c/two", b"1"), ("C/TWO", b"2")])
            .is_err());
        assert!(p.getattr(at("c/new")).unwrap().is_none());
        assert!(p.getattr(at("c/two")).unwrap().is_none());
        // Durable at the next durable point, and whole after a reopen.
        drop(p);
        s.close().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("batch").unwrap();
        assert_eq!(read_file(&p, "c/old"), b"new");
        assert_eq!(read_file(&p, "c/big"), big);
        assert!(s.store.verify().unwrap().is_ok());
    }

    #[test]
    fn folded_path_is_the_parsed_paths_fold() {
        for rel in [
            "",
            "a",
            "Data/Meshes/Actor.NIF",
            "Data\\SKSE\\Plugins/x.ini",
            "/lead//double///and/trail/",
            "\\\\server\\share",
            "ÄÖ/İstanbul/\u{212A}.txt",
            "a/./b",
            "a/../b",
            "..",
            ".",
            "a/.../b",
            "a/.hidden/..b",
        ] {
            assert_eq!(
                folded_path(rel),
                LPath::parse(rel).map(|p| p.folded),
                "{rel:?}"
            );
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
        let (s, d) = temp_storage_every_close();
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
        let (s, d) = temp_storage_every_close();
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
        let (s, d) = temp_storage_every_close();
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
        let (s, _d) = temp_storage_every_close();
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

    /// A layer `l` with provider `lp` and a closed file `f` holding `body`;
    /// returns the file's store id and GUID.
    fn layer_with_file(
        s: &Arc<Storage>,
        body: &[u8],
    ) -> (Arc<LayerProvider>, Arc<dyn Provider>, [u8; 17], [u8; 16]) {
        let lid = s.catalog.create_layer("l").unwrap();
        let lp: Arc<LayerProvider> = Arc::new(LayerProvider::new(Arc::clone(s), "l".into(), lid));
        let p: Arc<dyn Provider> = lp.clone();
        write_file(&p, "f", 0, body);
        let guid = s.catalog.get(lid, "f").unwrap().unwrap().guid;
        (lp, p, layer_file_id(&guid), guid)
    }

    /// What the block store holds for `id`: its whole length, with no block
    /// missing.
    fn stored(s: &Storage, id: &[u8; 17]) -> Vec<u8> {
        let len = s.store.stat(id).unwrap().expect("store file").len;
        let mut buf = vec![0u8; len as usize];
        let r = s.store.read(id, 0, &mut buf).unwrap();
        assert!(
            r.missing.is_empty(),
            "store blocks missing: {:?}",
            r.missing
        );
        assert_eq!(r.bytes, buf.len());
        buf
    }

    /// A grow whose block writes fail after the store was resized (disk
    /// full) puts the store back: the closed file's length and its tail
    /// block are what they were, so a crash right after loses nothing that
    /// was closed. The dirty block below the resize went in first.
    #[test]
    fn a_failed_grow_commit_keeps_the_closed_tail() {
        let (s, d) = temp_storage_every_close();
        let old: Vec<u8> = (0..(2 * BS + 100)).map(|i| (i % 251) as u8).collect();
        let (lp, p, id, guid) = layer_with_file(&s, &old);

        let (h, _, _) = p.open(at("f"), OPEN_WRITE).unwrap();
        p.write_at(h, 10, b"head").unwrap(); // block 0: below the resize
        p.write_at(h, 5 * BS, b"grown").unwrap();
        lp.live_cell(&guid)
            .unwrap()
            .fail_after_set_len
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(p.flush(h), Err(ST_IO_ERROR));

        let mut want_store = old.clone();
        want_store[10..14].copy_from_slice(b"head");
        assert_eq!(
            stored(&s, &id),
            want_store,
            "the store is back at the closed length"
        );
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        assert_eq!(
            s.catalog.get(lid, "f").unwrap().unwrap().len,
            old.len() as u64
        );

        // Killed now, after the store reached disk: the closed bytes are there.
        #[cfg(not(windows))]
        {
            s.store.flush().unwrap();
            let killed = tempfile::tempdir().unwrap();
            snapshot(d.path(), killed.path());
            let k = Storage::open(killed.path(), cfg()).unwrap();
            let r = k.last_reconcile();
            assert!(
                r.zero_filled_files.is_empty() && r.corrupt_files.is_empty(),
                "{r:?}"
            );
            let kp = k.layer("l").unwrap();
            assert_eq!(read_file(&kp, "f"), want_store);
        }
        #[cfg(windows)]
        let _ = d;

        // The handle still has its writes, and the next commit lands them.
        let mut want = want_store.clone();
        want.resize(5 * BS as usize, 0);
        want.extend_from_slice(b"grown");
        assert!(read_range(&p, h, 0, want.len() + 1) == want, "open handle");
        p.close(h).unwrap();
        assert!(read_file(&p, "f") == want, "after close");
    }

    /// A shrink to an unaligned length whose tail write fails after the
    /// resize puts every dropped block back.
    #[test]
    fn a_failed_shrink_commit_keeps_the_closed_bytes() {
        let (s, _d) = temp_storage();
        let old: Vec<u8> = (0..(5 * BS + 100)).map(|i| (i % 253) as u8).collect();
        let (lp, p, id, guid) = layer_with_file(&s, &old);

        let (h, _, _) = p.open(at("f"), OPEN_WRITE).unwrap();
        lp.live_cell(&guid)
            .unwrap()
            .fail_after_set_len
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(p.set_len(h, 2 * BS + 7), Err(ST_IO_ERROR));
        assert_eq!(stored(&s, &id), old);

        p.close(h).unwrap();
        assert!(
            read_file(&p, "f") == old[..(2 * BS + 7) as usize],
            "after close"
        );
    }

    /// A shrink that drops more blocks than a commit captures first shrinks
    /// the store to the block boundary above the new length (only bytes the
    /// handle already cut go); a failure after that restores the tail block,
    /// and the row follows the store's length.
    #[test]
    fn a_failed_large_shrink_commit_stops_at_the_block_boundary() {
        let (s, _d) = temp_storage();
        let old: Vec<u8> = (0..((crate::layer_io::MAX_CAPTURE_BLOCKS + 10) * BS + 100))
            .map(|i| (i % 249) as u8)
            .collect();
        let (lp, p, id, guid) = layer_with_file(&s, &old);

        let (h, _, _) = p.open(at("f"), OPEN_WRITE).unwrap();
        lp.live_cell(&guid)
            .unwrap()
            .fail_after_set_len
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(p.set_len(h, 100), Err(ST_IO_ERROR));
        assert_eq!(stored(&s, &id), old[..BS as usize]);
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        assert_eq!(s.catalog.get(lid, "f").unwrap().unwrap().len, BS);
        assert_eq!(p.getattr(at("f")).unwrap().unwrap().size, 100);

        p.close(h).unwrap();
        assert!(read_file(&p, "f") == old[..100], "after close");
        assert_eq!(s.catalog.get(lid, "f").unwrap().unwrap().len, 100);
    }

    #[test]
    fn put_files_failing_after_its_rows_keeps_their_data() {
        let (s, d) = temp_storage();
        let id = s.catalog.create_layer("l").unwrap();
        let lp = LayerProvider::new(Arc::clone(&s), "l".into(), id);
        let big: Vec<u8> = (0..2 * BS as usize + 3).map(|i| (i % 249) as u8).collect();
        lp.fail_after_rows
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            lp.put_files(&[("c/a", b"alpha"), ("c/big", &big)]),
            Err(ST_IO_ERROR)
        );
        // The rows were committed: their data is still there.
        let p: Arc<dyn Provider> = Arc::new(lp);
        assert_eq!(read_file(&p, "c/a"), b"alpha");
        assert_eq!(read_file(&p, "c/big"), big);
        drop(p);
        s.close().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("l").unwrap();
        assert_eq!(read_file(&p, "c/a"), b"alpha");
        assert_eq!(read_file(&p, "c/big"), big);
        assert!(s.store.verify().unwrap().is_ok());
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
        let (s, d) = temp_storage_every_close();
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

    /// The files a many-close test writes.
    const MANY: usize = 200;

    /// Deferred: closes, flushes and namespace changes make no durable point.
    /// `OnEveryClose`: each makes one.
    #[test]
    fn deferred_changes_make_no_durable_point_on_every_close_makes_one_each() {
        for (durability, per_op) in [(Durability::default(), 0), (Durability::OnEveryClose, 1)] {
            let (s, _d) = temp_storage_with(durability);
            let p = s.layer("l").unwrap();
            let before = s.clock.points();
            for i in 0..5 {
                write_file(&p, &format!("d/f{i}"), 0, b"body");
            }
            assert_eq!(
                s.clock.points() - before,
                5 * per_op,
                "{durability:?} closes"
            );
            let (h, _, _) = p.open(at("d/f0"), OPEN_WRITE).unwrap();
            p.write_at(h, 0, b"B").unwrap();
            p.flush(h).unwrap();
            assert_eq!(
                s.clock.points() - before,
                6 * per_op,
                "{durability:?} flush"
            );
            p.close(h).unwrap();
            p.mkdir(at("e")).unwrap();
            p.rename(at("d/f1"), at("e/f1")).unwrap();
            p.remove(at("d/f2")).unwrap();
            p.set_attr(
                at("d/f3"),
                SetAttr {
                    size: Some(1),
                    mtime: None,
                },
            )
            .unwrap();
            // (`OnEveryClose`: the close after the flush had nothing left to
            // make durable, so it skipped the fsyncs.)
            assert_eq!(
                s.clock.points() - before,
                10 * per_op,
                "{durability:?} namespace changes"
            );
            // A sync with changes pending is a durable point (under
            // `OnEveryClose` none are).
            s.sync().unwrap();
            assert_eq!(s.clock.points() - before, 10 * per_op + (1 - per_op));
        }
    }

    /// `sync` (which `close` runs) and a provider's drop skip the fsyncs
    /// when nothing changed since the last durable point.
    #[test]
    fn an_idle_sync_drop_or_close_makes_no_durable_point() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "gone", 0, b"x");
        p.remove(at("gone")).unwrap();
        s.sync().unwrap();
        // The removed file's store delete ran after that commit, so one
        // more durable point has something to publish; after it, nothing.
        s.sync().unwrap();
        let settled = s.clock.points();
        let (h, _, _) = p.open(at(""), OPEN_READ).unwrap(); // reads change nothing
        p.close(h).unwrap();
        s.sync().unwrap();
        drop(p);
        assert_eq!(s.clock.points(), settled, "idle sync and drop");
        let q = s.layer("l").unwrap();
        write_file(&q, "f", 0, b"y");
        s.sync().unwrap();
        assert_eq!(s.clock.points(), settled + 1, "a change makes it count");
    }

    /// A file open across a durable point makes one at its close (its row
    /// is durable now); in a scratch directory it does not, so many large
    /// temporaries written at once do not chain durable points.
    #[test]
    fn a_scratch_file_open_across_a_durable_point_makes_none_at_close() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(
            d.path(),
            StorageConfig {
                scratch_dirs: vec!["Tmp".into()],
                ..cfg()
            },
        )
        .unwrap();
        let p = s.layer("l").unwrap();
        p.mkdir(at("tmp")).unwrap();
        p.mkdir(at("keep")).unwrap();
        s.sync().unwrap();
        let (t, _, _) = p.open(at("TMP/a"), OPEN_WRITE | OPEN_CREATE).unwrap();
        let (k, _, _) = p.open(at("keep/a"), OPEN_WRITE | OPEN_CREATE).unwrap();
        p.write_at(t, 0, &[1; 3 * BS as usize]).unwrap();
        p.write_at(k, 0, &[2; 3 * BS as usize]).unwrap();
        s.sync().unwrap();
        let before = s.clock.points();
        p.write_at(t, 3 * BS, &[3; BS as usize]).unwrap();
        p.write_at(k, 3 * BS, &[4; BS as usize]).unwrap();
        p.close(t).unwrap();
        assert_eq!(
            s.clock.points(),
            before,
            "a scratch file's close is deferred"
        );
        p.close(k).unwrap();
        assert_eq!(
            s.clock.points(),
            before + 1,
            "any other file's close is not"
        );
        // Both read back whole, and do after a reopen.
        let mut buf = vec![0u8; 4 * BS as usize];
        let (h, _, _) = p.open(at("tmp/a"), OPEN_READ).unwrap();
        assert_eq!(p.read_at(h, 0, &mut buf).unwrap(), buf.len());
        assert_eq!(buf[3 * BS as usize], 3);
        p.close(h).unwrap();
        drop(p);
        drop(s);
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("l").unwrap();
        let (h, _, _) = p.open(at("tmp/a"), OPEN_READ).unwrap();
        assert_eq!(p.read_at(h, 0, &mut buf).unwrap(), buf.len());
        assert_eq!((buf[0], buf[3 * BS as usize]), (1, 3));
        p.close(h).unwrap();
    }

    /// Once the last durable point is `max_interval` old, the next change
    /// makes one, and the interval starts again.
    #[test]
    fn a_change_after_max_interval_makes_a_durable_point() {
        let max_interval = std::time::Duration::from_secs(60);
        let (s, _d) = temp_storage_with(Durability::Deferred { max_interval });
        let p = s.layer("l").unwrap();
        let before = s.clock.points();
        write_file(&p, "a", 0, b"a");
        s.clock.advance(max_interval / 2);
        write_file(&p, "b", 0, b"b");
        assert_eq!(s.clock.points(), before, "not yet due");
        s.clock.advance(max_interval / 2);
        write_file(&p, "c", 0, b"c");
        assert_eq!(s.clock.points(), before + 1, "due: the close made one");
        write_file(&p, "d", 0, b"d");
        assert_eq!(s.clock.points(), before + 1, "the interval restarted");
        s.clock.advance(max_interval);
        p.mkdir(at("x")).unwrap();
        assert_eq!(s.clock.points(), before + 2, "a namespace change too");
    }

    /// A due durable point run by a change deletes every live layer's
    /// deferred deletions, not only its own layer's.
    #[test]
    fn a_due_durable_point_covers_every_live_layer() {
        let max_interval = std::time::Duration::from_secs(60);
        let (s, _d) = temp_storage_with(Durability::Deferred { max_interval });
        let a = s.layer("a").unwrap();
        let b = s.layer("b").unwrap();
        write_file(&b, "gone", 0, b"x");
        let lid = s.catalog.layer_id("b").unwrap().unwrap();
        let id = layer_file_id(&s.catalog.get(lid, "gone").unwrap().unwrap().guid);
        b.remove(at("gone")).unwrap();
        assert!(s.store.stat(&id).unwrap().is_some(), "deferred");
        s.clock.advance(max_interval);
        write_file(&a, "f", 0, b"y");
        assert!(s.store.stat(&id).unwrap().is_none(), "deleted by a's point");
    }

    /// Deferred: a removed or replaced file's store data stays until a
    /// durable point has made the removal durable; `sync` deletes it.
    #[test]
    fn deferred_deletions_wait_for_a_durable_point() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "removed", 0, b"r");
        write_file(&p, "old", 0, b"o");
        write_file(&p, "new", 0, b"n");
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        let id_of = |path: &str| layer_file_id(&s.catalog.get(lid, path).unwrap().unwrap().guid);
        let removed = id_of("removed");
        let replaced = id_of("old");
        let (h, _, _) = p.open(at("new"), OPEN_READ).unwrap();
        p.remove(at("removed")).unwrap();
        p.rename(at("new"), at("old")).unwrap();
        p.close(h).unwrap();
        assert!(s.store.stat(&removed).unwrap().is_some(), "removed: kept");
        assert!(s.store.stat(&replaced).unwrap().is_some(), "replaced: kept");
        assert_eq!(read_file(&p, "old"), b"n");
        s.sync().unwrap();
        assert!(
            s.store.stat(&removed).unwrap().is_none(),
            "removed: deleted"
        );
        assert!(
            s.store.stat(&replaced).unwrap().is_none(),
            "replaced: deleted"
        );
        assert_eq!(read_file(&p, "old"), b"n");
    }

    /// No file was emptied, zero-filled, found corrupt or resized, and no
    /// repair failed: only orphans (and cache rows) may have been dropped.
    #[cfg(not(windows))]
    fn assert_no_file_repaired(r: &crate::ReconcileReport) {
        assert!(
            r.emptied_files.is_empty()
                && r.zero_filled_files.is_empty()
                && r.corrupt_files.is_empty()
                && r.resized_rows.is_empty()
                && r.failed_repairs.is_empty(),
            "{r:?}"
        );
    }

    /// Deferred, killed: what `sync` made durable is there; what came after
    /// may be gone, but only whole — the store reopens consistent, with no
    /// file emptied, zero-filled or corrupt, and a removal that was not yet
    /// durable brings its file back with all its data.
    #[cfg(not(windows))]
    #[test]
    fn deferred_crash_keeps_what_sync_made_durable_and_reopens_clean() {
        let (s, d) = temp_storage();
        let p = s.layer("l").unwrap();
        let big: Vec<u8> = (0..(3 * BS + 11)).map(|i| (i % 251) as u8).collect();
        write_file(&p, "Kept/big.bin", 0, &big);
        write_file(&p, "kept/doomed.txt", 0, b"removed later");
        s.sync().unwrap();
        write_file(&p, "later/new.bin", 0, &big);
        write_file(&p, "save.tmp", 0, b"new save");
        p.rename(at("save.tmp"), at("kept/save.ess")).unwrap();
        p.remove(at("kept/doomed.txt")).unwrap();
        // The store half of the later writes reached disk; the catalog's did not.
        s.store.flush().unwrap();

        let (k, kp, _kd) = killed_copy(d.path(), "l");
        let r = k.last_reconcile();
        assert_no_file_repaired(r);
        assert!(r.orphans_deleted >= 2, "the unpublished files' data: {r:?}");
        assert_eq!(names(&kp, ""), ["Kept"]);
        assert_eq!(names(&kp, "kept"), ["big.bin", "doomed.txt"]);
        assert_eq!(read_file(&kp, "kept/big.bin"), big);
        assert_eq!(read_file(&kp, "kept/doomed.txt"), b"removed later");
        drop(kp);
        k.close().unwrap();

        // The same, after a sync: everything is there. (The removed file's
        // store delete, made after the durable commit, is itself not durable
        // yet: reconciliation deletes that data as an orphan.)
        s.sync().unwrap();
        let (k, kp, _kd) = killed_copy(d.path(), "l");
        assert_no_file_repaired(k.last_reconcile());
        assert_eq!(names(&kp, "kept"), ["big.bin", "save.ess"]);
        assert_eq!(read_file(&kp, "kept/save.ess"), b"new save");
        assert_eq!(read_file(&kp, "later/new.bin"), big);
    }

    /// Deferred, dropped without `Storage::close`: the provider's drop is a
    /// durable point, so nothing closed is lost.
    #[test]
    fn deferred_writes_survive_a_drop_without_close() {
        let (s, d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "a", 0, b"after no sync");
        drop(p);
        drop(s);
        let s = Storage::open(d.path(), cfg()).unwrap();
        assert_eq!(*s.last_reconcile(), Default::default());
        assert_eq!(read_file(&s.layer("l").unwrap(), "a"), b"after no sync");
    }

    /// Deferred: rewriting a file that is already durable, in place, with
    /// the block store flushing in the middle (its auto-flush), then a crash
    /// after the close: the file comes back whole (the close made a durable
    /// point), never emptied or torn.
    #[cfg(not(windows))]
    #[test]
    fn deferred_rewrite_of_a_durable_file_survives_a_mid_rewrite_store_flush() {
        let (s, d) = temp_storage();
        let lp = s.layer_provider("l", true).unwrap();
        let p: Arc<dyn Provider> = lp.clone();
        let old: Vec<u8> = (0..(3 * BS + 10)).map(|i| (i % 251) as u8).collect();
        let new: Vec<u8> = (0..(5 * BS + 3)).map(|i| (i % 13) as u8).collect();
        write_file(&p, "trunc.bin", 0, &old);
        write_file(&p, "grow.bin", 0, &old[..(BS + 10) as usize]);
        s.sync().unwrap();

        // Truncate on open (commits the store's resize to 0), store flush,
        // then the new content.
        let (h, _, _) = p.open(at("trunc.bin"), OPEN_WRITE | OPEN_TRUNC).unwrap();
        s.store.flush().unwrap();
        p.write_at(h, 0, &new).unwrap();
        p.close(h).unwrap();

        // Grow: the close's commit resizes the store, which flushes before
        // the block writes.
        let (h, _, _) = p.open(at("grow.bin"), OPEN_WRITE).unwrap();
        p.write_at(h, 0, &new).unwrap();
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        let guid = s.catalog.get(lid, "grow.bin").unwrap().unwrap().guid;
        lp.live_cell(&guid)
            .unwrap()
            .flush_after_set_len
            .store(true, std::sync::atomic::Ordering::SeqCst);
        p.close(h).unwrap();

        let (k, kp, _kd) = killed_copy(d.path(), "l");
        assert_no_file_repaired(k.last_reconcile());
        for f in ["trunc.bin", "grow.bin"] {
            let got = read_file(&kp, f);
            assert!(got == new, "{f}: {} bytes, not the new content", got.len());
        }
    }

    /// Deferred: a new file renamed over a durable one, the store flushed,
    /// then a crash: the durable file is there, old or new, and whole.
    #[cfg(not(windows))]
    #[test]
    fn deferred_rename_over_a_durable_file_then_a_crash_keeps_a_whole_file() {
        let (s, d) = temp_storage();
        let p = s.layer("l").unwrap();
        let old = vec![0x11u8; 2 * BS as usize + 5];
        let new = vec![0x22u8; 3 * BS as usize + 7];
        write_file(&p, "save.ess", 0, &old);
        s.sync().unwrap();
        write_file(&p, "save.tmp", 0, &new);
        p.rename(at("save.tmp"), at("save.ess")).unwrap();
        s.store.flush().unwrap();

        let (k, kp, _kd) = killed_copy(d.path(), "l");
        assert_no_file_repaired(k.last_reconcile());
        assert_eq!(names(&kp, ""), ["save.ess"]);
        let got = read_file(&kp, "save.ess");
        assert!(got == old || got == new, "{} bytes", got.len());
    }

    /// Deferred: a durable point is also due once the catalog holds
    /// `max_commits` non-durable commits, whatever `max_interval` says.
    #[test]
    fn deferred_changes_make_a_durable_point_at_the_commit_bound() {
        let (s, _d) = temp_storage();
        s.clock.set_max_commits(20);
        let p = s.layer("l").unwrap();
        let before = s.clock.points();
        for i in 0..50 {
            write_file(&p, &format!("f{i}"), 0, b"x");
        }
        let made = s.clock.points() - before;
        assert!(
            (1..50).contains(&made),
            "{made} durable points for 50 closes"
        );
        assert!(s.catalog.unflushed_commits() < 20 + 10);
    }

    /// `sync` never holds a layer's provider: a dropped provider's layer
    /// can be deleted at once while other threads sync.
    #[test]
    fn delete_layer_after_drop_is_not_refused_while_syncing() {
        let (s, _d) = temp_storage();
        let other = s.layer("other").unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let syncer = {
            let (s, other, stop) = (Arc::clone(&s), Arc::clone(&other), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    write_file(&other, "o", 0, &i.to_le_bytes());
                    s.sync().unwrap();
                    i += 1;
                }
            })
        };
        for i in 0..100 {
            let p = s.layer("x").unwrap();
            write_file(&p, "f", 0, b"x");
            drop(p);
            if let Err(e) = s.delete_layer("x") {
                stop.store(true, std::sync::atomic::Ordering::Release);
                syncer.join().unwrap();
                panic!("iteration {i}: {e}");
            }
        }
        stop.store(true, std::sync::atomic::Ordering::Release);
        syncer.join().unwrap();
        drop(other);
    }

    /// Deferred: a file removed while open keeps its store data past its
    /// last close, until a durable point.
    #[test]
    fn deferred_remove_while_open_deletes_the_data_at_the_durable_point() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "gone.txt", 0, b"still readable");
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        let id = layer_file_id(&s.catalog.get(lid, "gone.txt").unwrap().unwrap().guid);
        let (h, _, _) = p.open(at("gone.txt"), OPEN_READ).unwrap();
        p.remove(at("gone.txt")).unwrap();
        assert!(p.getattr(at("gone.txt")).unwrap().is_none());
        assert_eq!(read_range(&p, h, 0, 14), b"still readable");
        p.close(h).unwrap();
        assert!(s.store.stat(&id).unwrap().is_some(), "deferred past close");
        s.sync().unwrap();
        assert!(s.store.stat(&id).unwrap().is_none(), "deleted by sync");
    }

    /// Many deferred closes: no durable point at all, and cheap.
    #[test]
    fn many_deferred_closes_make_no_durable_point() {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        let before = s.clock.points();
        let t = std::time::Instant::now();
        for i in 0..MANY {
            write_file(&p, &format!("d{}/f{i}.txt", i % 10), 0, b"small file");
        }
        let took = t.elapsed();
        assert_eq!(s.clock.points(), before);
        eprintln!("{MANY} deferred closes: {took:?}");
        s.sync().unwrap();
        assert_eq!(s.clock.points(), before + 1);
    }

    /// Timing comparison of the two policies (fsync cost depends on the
    /// filesystem, so it only reports): `cargo test -p vfs-storage --
    /// --ignored --nocapture deferred_vs_every_close`.
    #[test]
    #[ignore]
    fn deferred_vs_every_close_timing() {
        for durability in [Durability::default(), Durability::OnEveryClose] {
            let (s, _d) = temp_storage_with(durability);
            let p = s.layer("l").unwrap();
            let t = std::time::Instant::now();
            for i in 0..MANY {
                write_file(&p, &format!("d{}/f{i}.txt", i % 10), 0, b"small file");
            }
            s.sync().unwrap();
            eprintln!("{durability:?}: {MANY} closes + sync: {:?}", t.elapsed());
        }
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

    /// Installs a gate as the storage's layer read hook: every read that
    /// enters waits there — holding the file's state lock — until `want`
    /// reads are inside at once. Reads that exclude each other never get
    /// there; the first to wait `patience` gives up for all of them, so the
    /// caller's assertion on the returned peak fails instead of the test
    /// hanging. Returns the gate: `(inside, peak, gave_up)`.
    #[allow(clippy::type_complexity)]
    fn gate_reads(
        s: &Storage,
        want: usize,
        patience: Duration,
    ) -> Arc<(Mutex<(usize, usize, bool)>, Condvar)> {
        let gate = Arc::new((Mutex::new((0usize, 0usize, false)), Condvar::new()));
        let g = Arc::clone(&gate);
        *s.layer_read_hook.lock().unwrap() = Some(Arc::new(move || {
            let (m, cv) = &*g;
            let mut st = m.lock().unwrap();
            st.0 += 1;
            st.1 = st.1.max(st.0);
            cv.notify_all();
            let (mut st, timeout) = cv
                .wait_timeout_while(st, patience, |st| st.1 < want && !st.2)
                .unwrap();
            if timeout.timed_out() {
                st.2 = true;
                cv.notify_all();
            }
            st.0 -= 1;
        }));
        gate
    }

    #[test]
    fn reads_of_one_file_run_concurrently() {
        // The game issues eight 1 MiB reads of one plugin at once. Each read
        // holds the file's state lock shared, so all eight are inside the
        // read together — on one handle or several.
        const N: usize = 8;
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        let body: Vec<u8> = (0..N * BS as usize)
            .map(|i| (i / BS as usize) as u8 + 1)
            .collect();
        write_file(&p, "big.bin", 0, &body);
        let (h1, _, _) = p.open(at("big.bin"), OPEN_READ).unwrap();
        let (h2, _, _) = p.open(at("big.bin"), OPEN_READ).unwrap();

        let gate = gate_reads(&s, N, Duration::from_secs(5));
        std::thread::scope(|sc| {
            for i in 0..N {
                let p = &p;
                sc.spawn(move || {
                    let h = if i % 2 == 0 { h1 } else { h2 };
                    let mut buf = vec![0u8; BS as usize];
                    let n = p.read_at(h, i as u64 * BS, &mut buf).unwrap();
                    assert_eq!(n, BS as usize);
                    assert!(buf.iter().all(|&b| b == i as u8 + 1), "block {i}");
                });
            }
        });
        *s.layer_read_hook.lock().unwrap() = None;
        let (_, peak, gave_up) = *gate.0.lock().unwrap();
        assert!(!gave_up, "reads of one file excluded each other");
        assert_eq!(peak, N, "all {N} reads were inside the read at once");
        p.close(h1).unwrap();
        p.close(h2).unwrap();
    }

    /// A hook that parks every call until [`Park::release`]: installed as
    /// one of the storage's layer hooks, it holds reads at that point.
    #[derive(Default)]
    struct Park {
        /// (calls parked or past, released)
        st: Mutex<(usize, bool)>,
        cv: Condvar,
    }

    impl Park {
        fn hook(self: &Arc<Self>) -> Arc<dyn Fn() + Send + Sync> {
            let park = Arc::clone(self);
            Arc::new(move || {
                let mut st = park.st.lock().unwrap();
                st.0 += 1;
                park.cv.notify_all();
                let _released = park.cv.wait_while(st, |st| !st.1).unwrap();
            })
        }

        /// Waits until `n` calls are parked; false if they are not within
        /// `patience` (calls that exclude each other never all arrive).
        fn wait_for(&self, n: usize, patience: Duration) -> bool {
            let (_st, timeout) = self
                .cv
                .wait_timeout_while(self.st.lock().unwrap(), patience, |st| st.0 < n)
                .unwrap();
            !timeout.timed_out()
        }

        fn release(&self) {
            self.st.lock().unwrap().1 = true;
            self.cv.notify_all();
        }
    }

    #[test]
    fn a_change_waits_for_a_read_in_flight() {
        // A read holds the state shared; everything that changes the file
        // takes it exclusive, so none of it lands while a read is inside —
        // the read returns the bytes from before the change.
        type Change = fn(&Arc<dyn Provider>, u64);
        let changes: [(&str, Change, &[u8]); 6] = [
            (
                "write_at",
                |p, w| {
                    p.write_at(w, 0, &[2u8; 4]).unwrap();
                },
                &[2, 2, 2, 2, 1, 1],
            ),
            ("set_len", |p, w| p.set_len(w, 3).unwrap(), &[1, 1, 1]),
            ("flush", |p, w| p.flush(w).unwrap(), &[1, 1, 1, 1, 1, 1]),
            // A close commits the handle's unflushed byte.
            ("close", |p, w| p.close(w).unwrap(), &[1, 1, 1, 1, 1, 1]),
            (
                "open with OPEN_TRUNC",
                |p, _| {
                    let (t, _, _) = p.open(at("f.bin"), OPEN_WRITE | OPEN_TRUNC).unwrap();
                    p.close(t).unwrap();
                },
                &[],
            ),
            (
                "set_attr size",
                |p, _| {
                    let attr = SetAttr {
                        size: Some(2),
                        mtime: None,
                    };
                    p.set_attr(at("f.bin"), attr).unwrap();
                },
                &[1, 1],
            ),
        ];
        for (what, change, after) in changes {
            let (s, _d) = temp_storage();
            let p = s.layer("l").unwrap();
            write_file(&p, "f.bin", 0, &[1u8; 6]);
            let (w, _, _) = p.open(at("f.bin"), OPEN_WRITE).unwrap();
            let (r, _, _) = p.open(at("f.bin"), OPEN_READ).unwrap();
            // Unflushed bytes, so the flush and the close have a commit to run.
            p.write_at(w, 5, &[1u8]).unwrap();

            // The reader parks inside the read until released.
            let park = Arc::new(Park::default());
            *s.layer_read_hook.lock().unwrap() = Some(park.hook());
            let changed = AtomicBool::new(false);
            let (early, read) = std::thread::scope(|sc| {
                let reader = sc.spawn(|| {
                    let mut buf = [0u8; 6];
                    let n = p.read_at(r, 0, &mut buf).unwrap();
                    buf[..n].to_vec()
                });
                let parked = park.wait_for(1, Duration::from_secs(5));
                // Later reads (the checks below) must not park.
                *s.layer_read_hook.lock().unwrap() = None;
                sc.spawn(|| {
                    change(&p, w);
                    changed.store(true, Ordering::SeqCst);
                });
                // The change cannot finish while the read is parked. (A sleep
                // can only miss a change that wrongly got through late, never
                // fail a correct run.) The reader is released before anything
                // is asserted, so a failure fails the test instead of hanging
                // the scope on a reader nobody releases.
                std::thread::sleep(Duration::from_millis(100));
                let early = changed.load(Ordering::SeqCst);
                park.release();
                assert!(parked, "{what}: the read never reached the hook");
                (early, reader.join().unwrap())
            });
            assert!(!early, "{what} landed while a read held the file's state");
            assert_eq!(read, [1u8; 6], "{what}: the read");
            assert!(changed.load(Ordering::SeqCst));
            assert_eq!(read_range(&p, r, 0, 16), after, "after {what}");
            p.close(r).unwrap();
            if what != "close" {
                p.close(w).unwrap();
            }
        }
    }

    #[test]
    fn concurrent_cold_fills_of_one_block_finish_before_a_commit() {
        // The case the shared lock exists for, and the one it must not get
        // wrong: several reads miss the RAM tier on the same block and fill
        // it from the store at once, with a writer waiting behind them.
        //
        // Every reader is held between its store read and its put into the
        // tier, so all N hold the block's *old* bytes, under the shared lock,
        // at the same moment — by construction, not by timing. The commit
        // cannot start until each has put; it then drops their blocks and
        // puts its own. If a fill ran outside the lock, or a commit did not
        // exclude readers, the commit would land while the readers are held
        // and their puts would leave the old block in the tier behind it.
        const N: usize = 4;
        let (s, d) = temp_storage();
        let p = s.layer("l").unwrap();
        let old: Vec<u8> = (0..2 * BS as usize).map(|i| (i % 251) as u8).collect();
        write_file(&p, "c.bin", 0, &old);
        // Reopen: the tier is cold, so every read below fills from the store.
        drop(p);
        drop(s);
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("l").unwrap();
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        let id = layer_file_id(&s.catalog.get(lid, "c.bin").unwrap().unwrap().guid);
        assert!(s.ram.get(&id, 0).is_none(), "the tier starts cold");
        let misses = s.ram.stats().misses;

        let (r, _, _) = p.open(at("c.bin"), OPEN_READ).unwrap();
        let (w, _, _) = p.open(at("c.bin"), OPEN_WRITE).unwrap();
        let park = Arc::new(Park::default());
        *s.layer_fill_hook.lock().unwrap() = Some(park.hook());
        let changed = AtomicBool::new(false);
        let new = vec![7u8; BS as usize];
        let (all_filling, early, reads) = std::thread::scope(|sc| {
            let readers: Vec<_> = (0..N)
                .map(|_| {
                    sc.spawn(|| {
                        let mut buf = vec![0u8; BS as usize];
                        let n = p.read_at(r, 0, &mut buf).unwrap();
                        buf.truncate(n);
                        buf
                    })
                })
                .collect();
            let all_filling = park.wait_for(N, Duration::from_secs(5));
            // The writer's own load of the block must not park.
            *s.layer_fill_hook.lock().unwrap() = None;
            sc.spawn(|| {
                p.write_at(w, 0, &new).unwrap();
                p.flush(w).unwrap();
                changed.store(true, Ordering::SeqCst);
            });
            std::thread::sleep(Duration::from_millis(100));
            let early = changed.load(Ordering::SeqCst);
            park.release();
            let reads: Vec<Vec<u8>> = readers.into_iter().map(|h| h.join().unwrap()).collect();
            (all_filling, early, reads)
        });
        assert!(all_filling, "{N} fills of one block never overlapped");
        assert!(!early, "the commit landed while readers were mid-fill");
        for got in &reads {
            assert_eq!(got[..], old[..BS as usize], "a reader's bytes");
        }
        // Each reader missed the tier and went to the store; the writer's
        // load of the block was a hit on what they filled.
        assert_eq!(s.ram.stats().misses - misses, N as u64);
        assert!(changed.load(Ordering::SeqCst));
        // After the commit the tier holds the new block, not a reader's.
        assert_eq!(s.ram.get(&id, 0).expect("the committed block")[..], new[..]);
        assert_eq!(read_range(&p, r, 0, BS as usize), new);
        assert_eq!(
            read_range(&p, r, BS, BS as usize)[..],
            old[BS as usize..],
            "the block the write did not touch"
        );
        p.close(r).unwrap();
        p.close(w).unwrap();
    }

    #[test]
    fn concurrent_reads_see_whole_changes_and_never_an_older_one() {
        // Readers race a writer that rewrites, truncates, flushes and
        // reopens one file. Every version is one byte value repeated, so a
        // read that saw part of a change shows two values; and a read that
        // began after a change completed must not see an older version (a
        // reader's stale RAM-tier refill landing after a commit would).
        //
        // Run three ways. With the default tier every read is a RAM hit on
        // the blocks the last commit put, so that run only covers the state
        // lock. A tier of two blocks holds a third of the file: readers
        // miss, fill from the store and evict each other constantly, which
        // is where a stale refill could happen. With no tier every read goes
        // to the store.
        const LEN: usize = 5 * BS as usize + 100; // 6 dirty blocks: write_at commits
        const SHORT: usize = BS as usize / 2;
        const LAST: u8 = 150;
        for ram_tier_bytes in [cfg().ram_tier_bytes, 2 * BS, 0] {
            let d = tempfile::tempdir().unwrap();
            let s = Storage::open(
                d.path(),
                StorageConfig {
                    ram_tier_bytes,
                    ..cfg()
                },
            )
            .unwrap();
            let p = s.layer("l").unwrap();
            write_file(&p, "v.bin", 0, &vec![1u8; LEN]);
            let evicts_before = s.ram.stats().evicts;
            // The newest version whose write has completed.
            let floor = AtomicU8::new(1);
            let done = AtomicBool::new(false);
            std::thread::scope(|sc| {
                for t in 0..6u64 {
                    let (p, floor, done) = (&p, &floor, &done);
                    sc.spawn(move || {
                        let (h, _, _) = p.open(at("v.bin"), OPEN_READ).unwrap();
                        // Readers start at different offsets: 0, mid-block and
                        // a block boundary, the last two past the truncated
                        // length.
                        let off = [0, 0, BS + 10, BS + 10, 2 * BS, 0][t as usize];
                        let mut buf = vec![0u8; LEN + BS as usize];
                        let mut reads = 0u64;
                        while !done.load(Ordering::SeqCst) || reads < 50 {
                            let before = floor.load(Ordering::SeqCst);
                            let n = p.read_at(h, off, &mut buf).unwrap();
                            reads += 1;
                            let full = LEN - off as usize;
                            let short = SHORT.saturating_sub(off as usize);
                            assert!(n == full || n == short, "read of {n} bytes at {off}");
                            if n == 0 {
                                continue;
                            }
                            let v = buf[0];
                            assert!(
                                buf[..n].iter().all(|&b| b == v),
                                "a read at {off} saw two versions (tier {ram_tier_bytes})"
                            );
                            assert!(
                                v >= before,
                                "read version {v} after {before} completed (tier {ram_tier_bytes})"
                            );
                        }
                        p.close(h).unwrap();
                    });
                }
                let (mut w, _, _) = p.open(at("v.bin"), OPEN_WRITE).unwrap();
                for v in 2..=LAST {
                    // Shrink first, so the rewrite below never leaves old
                    // bytes past a shorter length; a reader in between sees
                    // the old version cut short, which is a whole state.
                    if v % 3 == 0 {
                        p.set_len(w, SHORT as u64).unwrap();
                    }
                    // One call replaces the whole file: exclusive, so atomic
                    // to readers. From a truncated file it also regrows it.
                    let len = if v % 5 == 0 { SHORT } else { LEN };
                    if len == SHORT {
                        // A short rewrite of a long file would keep the old
                        // tail.
                        p.set_len(w, SHORT as u64).unwrap();
                    }
                    p.write_at(w, 0, &vec![v; len]).unwrap();
                    floor.store(v, Ordering::SeqCst);
                    if v % 4 == 0 {
                        p.flush(w).unwrap();
                    }
                    if v % 7 == 0 {
                        p.close(w).unwrap();
                        w = p.open(at("v.bin"), OPEN_WRITE).unwrap().0;
                    }
                }
                p.close(w).unwrap();
                done.store(true, Ordering::SeqCst);
            });
            assert_eq!(read_file(&p, "v.bin"), vec![LAST; SHORT]);
            // The small tier really was filled and evicted by the run (so
            // readers did refill from the store), and the default one never
            // had to evict.
            let evicted = s.ram.stats().evicts - evicts_before;
            match ram_tier_bytes {
                0 => assert_eq!(s.ram.stats().blocks, 0, "no tier holds nothing"),
                b if b == 2 * BS => assert!(evicted > 100, "tier of 2 blocks: {evicted} evictions"),
                _ => assert_eq!(evicted, 0, "the default tier never evicts here"),
            }
        }
    }

    #[test]
    fn remove_while_open_defers_the_store_delete_to_last_close() {
        let (s, _d) = temp_storage_every_close();
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
        let (s, _d) = temp_storage_every_close();
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

    // ---- names: what a listing and a final path spell ---------------------

    /// A base with `Data/Interface/Fonts/a.ttf` and `Data/Skyrim.ini`, under
    /// an overlay whose upper is a layer: what a game's root is.
    fn game_overlay(s: &Arc<Storage>) -> (Arc<dyn Provider>, Arc<dyn Provider>) {
        let base: Arc<dyn Provider> = Arc::new(vfs_compose::MemoryProvider::new());
        base.mkdir(at("Data")).unwrap();
        base.mkdir(at("Data/Interface")).unwrap();
        base.mkdir(at("Data/Interface/Fonts")).unwrap();
        write_file(&base, "Data/Interface/Fonts/a.ttf", 0, b"font");
        write_file(&base, "Data/Skyrim.ini", 0, b"[General]");
        let upper = s.layer("write").unwrap();
        let ov: Arc<dyn Provider> =
            Arc::new(vfs_compose::OverlayProvider::from_arcs(base, Arc::clone(&upper)).unwrap());
        (ov, upper)
    }

    fn listed(p: &Arc<dyn Provider>, dir: &str) -> Vec<String> {
        p.readdir(at(dir))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect()
    }

    /// How `p` spells each component of `path`, by the one-name lookup.
    fn spelled(p: &Arc<dyn Provider>, path: &str) -> String {
        let mut out = Vec::new();
        let mut prefix = String::new();
        for comp in path.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(comp);
            let name = vfs_compose::stored_name(p.as_ref(), at(&prefix)).unwrap();
            out.push(name.unwrap_or_else(|| format!("<{comp}?>")));
        }
        out.join("/")
    }

    /// **The spelling of what the base has does not change when the game
    /// writes under it.** The shim used to send folded paths, so the first
    /// write created `data` in the write layer; the merged listing took the
    /// upper's entry, name included, and `Data` became `data` for every
    /// caller from then on. A directory's final path taken before the write
    /// was no longer a prefix of a file's taken after it.
    #[test]
    fn a_write_under_a_base_directory_does_not_respell_it() {
        let (s, _d) = temp_storage();
        let (ov, upper) = game_overlay(&s);
        let fonts_before = spelled(&ov, "data/interface/fonts");
        assert_eq!(fonts_before, "Data/Interface/Fonts");

        // A write the way a folding client sends it: lower case throughout.
        ov.mkdir(at("data/skse")).unwrap();
        write_file(&ov, "data/skse/new log.txt", 0, b"log");
        write_file(&ov, "data/interface/fonts/b.ttf", 0, b"font2");
        // The upper now has its own, lower-case, rows for those directories.
        assert_eq!(listed(&upper, ""), ["data"]);

        assert_eq!(listed(&ov, ""), ["Data"], "the base's spelling stands");
        assert_eq!(listed(&ov, "Data"), ["Interface", "skse", "Skyrim.ini"]);
        assert_eq!(listed(&ov, "data/interface"), ["Fonts"]);
        let file_after = spelled(&ov, "data/interface/fonts/a.ttf");
        assert_eq!(file_after, "Data/Interface/Fonts/a.ttf");
        assert!(
            file_after.starts_with(&format!("{fonts_before}/")),
            "{fonts_before} (before the write) must still prefix {file_after} (after it)"
        );
        // The upper's entry is still the live one for everything but the name.
        write_file(&ov, "DATA/SKYRIM.INI", 0, b"[General]\nlonger now");
        let ini = ov.readdir(at("data")).unwrap();
        let ini = ini.iter().find(|e| e.name == "Skyrim.ini").unwrap();
        assert_eq!(ini.stat.size, 20, "the stat is the written copy's");
    }

    /// **A name the game creates is the name it gets back**, in a listing and
    /// from the one-name lookup, however it is asked for afterwards — as on
    /// NTFS. Reopening it in another case, for writing or with a
    /// create-or-truncate, does not respell it either.
    #[test]
    fn a_created_name_keeps_the_spelling_it_was_created_with() {
        let (s, _d) = temp_storage();
        let (ov, _upper) = game_overlay(&s);
        const SAVE: &str =
            "Save12_ABCDEF01_0_4E6F726420486572_Tamriel_000123_20261002150000_1_1.ess";
        ov.mkdir(at("Saves")).unwrap();
        write_file(&ov, &format!("saves/{SAVE}"), 0, b"save");
        ov.mkdir(at("Data/SKSE")).unwrap();
        write_file(&ov, "DATA/skse/CommunityShaders.log", 0, b"log");

        assert_eq!(listed(&ov, ""), ["Data", "Saves"]);
        assert_eq!(listed(&ov, "SAVES"), [SAVE]);
        assert_eq!(listed(&ov, "data/skse"), ["CommunityShaders.log"]);
        assert_eq!(
            spelled(&ov, &format!("saves/{}", SAVE.to_lowercase())),
            format!("Saves/{SAVE}")
        );
        assert_eq!(
            spelled(&ov, "data/skse/communityshaders.log"),
            "Data/SKSE/CommunityShaders.log"
        );

        // Opened again in other cases: for append, and create-or-truncate.
        write_file(&ov, "data/SKSE/COMMUNITYSHADERS.LOG", 3, b"more");
        let (h, _, _) = ov
            .open(
                at("Data/Skse/communityshaders.LOG"),
                OPEN_WRITE | OPEN_CREATE | vfs_provider::OPEN_TRUNC,
            )
            .unwrap();
        ov.close(h).unwrap();
        assert_eq!(listed(&ov, "data/skse"), ["CommunityShaders.log"]);
    }

    /// A rename that changes only the letter case respells the entry and
    /// nothing else: the bytes are there, nothing is hidden, and no whiteout
    /// is left behind to hide the file under its own name.
    #[test]
    fn a_rename_to_another_case_respells_the_entry() {
        let (s, _d) = temp_storage();
        let (ov, upper) = game_overlay(&s);
        ov.mkdir(at("Saves")).unwrap();
        write_file(&ov, "Saves/Quick.ess", 0, b"save");

        ov.rename(at("saves/quick.ess"), at("saves/QUICK.ESS"))
            .unwrap();
        assert_eq!(listed(&ov, "saves"), ["QUICK.ESS"]);
        assert_eq!(read_file(&ov, "Saves/quick.ess"), b"save");
        assert_eq!(listed(&upper, "saves"), ["QUICK.ESS"], "and no whiteout");

        // A directory, with something in it.
        ov.rename(at("SAVES"), at("saves")).unwrap();
        assert_eq!(listed(&ov, ""), ["Data", "saves"]);
        assert_eq!(read_file(&ov, "Saves/Quick.ess"), b"save");

        // To a different name altogether: spelled as the destination was.
        ov.rename(at("saves/quick.ess"), at("saves/Slot One.ESS"))
            .unwrap();
        assert_eq!(listed(&ov, "saves"), ["Slot One.ESS"]);

        // Something only the base has keeps the base's spelling, like every
        // name the base has; the rename succeeds and hides nothing.
        ov.rename(at("Data/Skyrim.ini"), at("Data/SKYRIM.INI"))
            .unwrap();
        assert_eq!(listed(&ov, "data"), ["Interface", "Skyrim.ini"]);
        assert_eq!(read_file(&ov, "data/skyrim.ini"), b"[General]");
        assert!(upper.getattr(at("Data/.wh.Skyrim.ini")).unwrap().is_none());
    }

    /// The created spelling is in the catalog row, so it is there after the
    /// storage is closed and opened again; and a row written before any of
    /// this, whose name is the folded one, still reads as that.
    #[test]
    fn created_spellings_survive_a_reopen_and_folded_rows_still_read() {
        let d = tempfile::tempdir().unwrap();
        {
            let s = Storage::open(d.path(), cfg()).unwrap();
            let p = s.layer("write").unwrap();
            p.mkdir(at("Saves")).unwrap();
            write_file(&p, "Saves/Quick.ess", 0, b"save");
            // What every existing layer holds: names as the shim folded them.
            p.mkdir(at("data")).unwrap();
            write_file(&p, "data/old log.txt", 0, b"old");
            drop(p);
            s.sync().unwrap();
            s.close().unwrap();
        }
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("write").unwrap();
        assert_eq!(listed(&p, ""), ["data", "Saves"]);
        assert_eq!(listed(&p, "saves"), ["Quick.ess"]);
        assert_eq!(listed(&p, "DATA"), ["old log.txt"]);
        assert_eq!(spelled(&p, "SAVES/QUICK.ESS"), "Saves/Quick.ess");
        assert_eq!(read_file(&p, "Data/Old Log.txt"), b"old");
    }
}
