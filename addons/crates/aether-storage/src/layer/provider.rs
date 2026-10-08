//! `impl Provider for LayerProvider`.

use super::*;

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
        let ns = lock_status(&self.ns)?;
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
        let of = self.handles.remove(h)?;
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
            let _ns = lock_status(&self.ns)?;
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
            let _ns = lock_status(&self.ns)?;
            let rec = self.get(&p.folded)?.ok_or_else(not_found)?;
            self.storage
                .catalog
                .remove(self.id, &p.folded)
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
            if rec.kind == KIND_FILE
                && let Some(c) = self.live_cell(&rec.guid)
            {
                *lock_status(&c.mtime_override)? = Some(mtime);
            }
        }
        Ok(())
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
