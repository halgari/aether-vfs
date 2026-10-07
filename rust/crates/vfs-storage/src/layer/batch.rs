//! [`LayerProvider::put_files`]: many whole files in one commit.

use super::*;

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
                let _ns = lock_status(&self.ns)?;
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
                let _ns = lock_status(&self.ns)?;
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
                    .put_many(self.id, &rows)
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
                    self.fresh.created(&self.storage.clock, *g)?;
                }
                Ok(())
            })();
            if let Err(e) = rows {
                if !committed {
                    // No row names them: their data goes now (or, if this
                    // fails too, at the next open's reconciliation).
                    for id in &ids {
                        if self.storage.store.delete(id).is_err() {
                            self.storage.needs_reconcile(
                                "rolling back a failed batch's store files failed",
                            );
                        }
                    }
                }
                return Err(e);
            }
        }
        self.changed(None)
    }
}
