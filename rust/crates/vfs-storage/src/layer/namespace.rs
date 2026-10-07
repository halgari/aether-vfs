//! The namespace half of a layer: parent directories, creating, dooming
//! and renaming rows.

use super::*;

impl LayerProvider {
    /// Creates every missing parent directory of `p` and returns the folded
    /// paths it created, outermost first, for [`Self::rollback`] if the
    /// operation that needed them fails. Under `ns`.
    pub(super) fn ensure_parents(&self, p: &LPath) -> Result<Vec<String>, i32> {
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
    pub(super) fn rollback(&self, created: &[String]) {
        for dir in created.iter().rev() {
            let _ = self.storage.catalog.remove(self.id, dir);
        }
    }

    /// The row of `guid` is gone. Under `ns`.
    pub(super) fn doom(&self, guid: Guid) -> Result<(), i32> {
        self.storage.ram.invalidate_file(&layer_file_id(&guid));
        if let Some(c) = self.live_cell(&guid) {
            *lock_status(&c.path)? = None;
            if c.opens.load(Ordering::Acquire) > 0 {
                return Ok(()); // its last `release` dooms it
            }
        }
        lock_status(&self.storage.doomed)?.push(guid);
        Ok(())
    }

    /// Creates the file row at `p` (and any missing parents) and its store
    /// file, and acquires its cell. On failure everything it created is
    /// rolled back. Under `ns`.
    pub(super) fn create(&self, p: &LPath) -> Result<Arc<FileCell>, i32> {
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
            self.fresh.created(&self.storage.clock, guid)?;
            self.acquire(&rec, &p.folded)
        })();
        if made.is_err() {
            let unrow = !row
                || self
                    .storage
                    .catalog
                    .remove(self.id, &p.folded)
                    .is_ok();
            let unstore = !stored || self.storage.store.delete(&id).is_ok();
            if !(unrow && unstore) {
                self.storage
                    .needs_reconcile("rolling back a failed layer file create failed");
            }
            self.rollback(&parents);
        }
        made
    }

    /// The namespace half of `rename`, under `ns`.
    pub(super) fn rename_rows(&self, from: &LPath, to: &LPath) -> Result<(), i32> {
        let _ns = lock_status(&self.ns)?;
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
        for c in lock_status(&self.cells)?.values().filter_map(Weak::upgrade) {
            let mut path = lock_status(&c.path)?;
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

    pub(super) fn set_attr_impl(&self, p: VPath, attr: SetAttr) -> Result<(), i32> {
        let p = LPath::parse(p.rel)?;
        if let Some(size) = attr.size {
            let ns = lock_status(&self.ns)?;
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
            let _ns = lock_status(&self.ns)?;
            let mut rec = self.get(&p.folded)?.ok_or_else(not_found)?;
            rec.mtime = mtime;
            self.put(&p.folded, &rec)?;
            if rec.kind == KIND_FILE {
                if let Some(c) = self.live_cell(&rec.guid) {
                    *lock_status(&c.mtime_override)? = Some(mtime);
                }
            }
        }
        Ok(())
    }
}
