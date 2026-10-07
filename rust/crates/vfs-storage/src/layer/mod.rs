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
//! **Durability (spec §6).** When a change becomes durable, and the exceptions,
//! are written once, in `rust/docs/durability.md` (module `crate::durable`);
//! `changed` here only calls [`Storage::after_change`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, Weak};

use vfs_core::fold;
use vfs_provider::{
    bad_request, exists, is_dir, map_io_err, not_a_dir, not_found, Access, Capabilities, CaseMatch,
    DirEntry, Handle, HandleTable, Provider, SetAttr, Stat, VPath, KIND_DIR, KIND_FILE,
    OPEN_CREATE, OPEN_EXCL, OPEN_TRUNC, OPEN_WRITE,
};

mod batch;
mod namespace;
mod path;
mod provider;

use crate::catalog::EntryRec;
use crate::durable::FreshFiles;
use crate::ids::{layer_file_id, new_guid, Guid};
use crate::layer_io::{FileCell, FileState};
use crate::storage::{Storage, StorageError};

use crate::util::lock_status;
use path::{folded_path, LPath};

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
    handles: HandleTable<Arc<OpenFile>>,
    /// Files created since the last durable point (the policy is in
    /// [`crate::durable`]).
    fresh: FreshFiles,
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
            handles: HandleTable::new(),
            fresh: FreshFiles::new(),
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
            .put(self.id, folded, rec)
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

    /// The shared cell of the file row `rec` at `folded`, created on first
    /// use, with one more open counted. Under `ns`.
    fn acquire(&self, rec: &EntryRec, folded: &str) -> Result<Arc<FileCell>, i32> {
        let mut cells = lock_status(&self.cells)?;
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
                            self.storage
                                .needs_reconcile("a layer file's row and store lengths differ");
                        }
                        info.len
                    }
                    Ok(None) => {
                        tracing::error!(
                            layer = %self.name, path = folded,
                            "layer file missing from the store: corruption"
                        );
                        self.storage
                            .needs_reconcile("a layer file is missing from the store");
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
        let _ns = lock_status(&self.ns)?;
        let last = cell.opens.fetch_sub(1, Ordering::AcqRel) == 1;
        if last && lock_status(&cell.path)?.is_none() {
            lock_status(&self.storage.doomed)?.push(cell.guid);
            return Ok(true);
        }
        Ok(false)
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
        let r = match cell.commit(&self.storage, &self.name, st) {
            Ok(true) => self.update_row(cell, st.len, true),
            Ok(false) => Ok(()),
            Err(e) => {
                if st.committed_len != before {
                    let _ = self.update_row(cell, st.committed_len, false);
                }
                Err(e)
            }
        };
        // A failed commit may leave the store's blocks or length and the row
        // apart (a later commit may mend it, but nothing ensures one runs).
        if r.is_err() {
            self.storage.needs_reconcile("a layer commit failed");
        }
        r
    }

    /// Sets `cell`'s row length to `len` (and its mtime, if `stamp`), if the
    /// row is still `cell`'s. Under the gate, which the caller holds.
    fn update_row(&self, cell: &FileCell, len: u64, stamp: bool) -> Result<(), i32> {
        let _ns = lock_status(&self.ns)?;
        let Some(path) = lock_status(&cell.path)?.clone() else {
            return Ok(());
        };
        if let Some(mut rec) = self.get(&path)? {
            if rec.guid == cell.guid {
                rec.len = len;
                if stamp {
                    rec.mtime = lock_status(&cell.mtime_override)?.unwrap_or_else(now);
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

    /// Called after a change that [`Durability::OnEveryClose`] makes durable
    /// before it returns; the policy is [`Storage::after_change`]. Called
    /// with no lock held.
    fn changed(&self, rewrote: Option<&FileCell>) -> Result<(), i32> {
        self.storage
            .after_change(&self.name, &self.fresh, rewrote)
            .map_err(|e| self.st_err("durable point", e))
    }

    fn handle(&self, h: Handle) -> Result<Arc<OpenFile>, i32> {
        self.handles.get(h)
    }

    fn file_of(&self, h: Handle) -> Result<(Arc<OpenFile>, Arc<FileCell>), i32> {
        let of = self.handle(h)?;
        let cell = of.cell.clone().ok_or_else(is_dir)?;
        Ok((of, cell))
    }

    fn track(&self, of: OpenFile) -> Result<Handle, i32> {
        self.handles.insert(Arc::new(of))
    }

    /// `set_len` on `cell` with its commit.
    fn resize(&self, cell: &FileCell, len: u64) -> Result<(), i32> {
        let mut st = write_state(&cell.state)?;
        cell.truncate(&self.storage, &self.name, &mut st, len)?;
        self.commit(cell, &mut st)
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
        let open: Vec<Arc<FileCell>> = self
            .handles
            .values()
            .unwrap_or_default()
            .iter()
            .filter_map(|of| of.cell.clone())
            .collect();
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
            let hook = crate::util::lock(&self.storage.drop_hook).take();
            if let Some(hook) = hook {
                hook();
            }
        }
        // `_leave` drops last: only now may the layer be deleted or get a new
        // provider.
    }
}

#[cfg(test)]
mod tests;
