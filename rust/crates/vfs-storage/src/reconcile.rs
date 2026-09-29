//! Reconciliation of the catalog with the block store at [`Storage::open`]
//! (spec §6).
//!
//! The catalog and the store commit separately. The write paths order them
//! (a file's row before its store file; the store flush before the durable
//! catalog commit; a row's removal before its store delete), so after a crash
//! the two can disagree only in ways this pass repairs:
//!
//! - a catalog file whose store file is missing (created, then killed before
//!   a store flush): recreated empty, its row's length set to 0, and logged;
//! - a `b'L'` or `b'C'` store id no catalog row names (a create whose row
//!   never became durable, or a delete that never reached the store): deleted;
//! - a cache row whose store file is missing: dropped, so the cache budget
//!   counts only what the store holds.
//!
//! Store ids of any other shape are not `vfs-storage`'s: they are logged and
//! left alone.

use std::collections::{HashMap, HashSet};

use vfs_block_store::{BlockStore, CompactOptions};

use crate::catalog::Catalog;
use crate::ids::{cache_file_id, classify_store_id, layer_file_id, StoreIdKind};
use crate::storage::{Storage, StorageError};

/// What reconciliation at [`Storage::open`] repaired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Layer files whose data was missing from the store, now empty:
    /// `(layer name, folded path)`.
    pub emptied_files: Vec<(String, String)>,
    /// Store files no catalog row referenced, deleted.
    pub orphans_deleted: u64,
    /// Cache rows whose store file was missing, dropped.
    pub cache_rows_dropped: u64,
}

/// Brings `catalog` and `store` back into agreement (see the module docs),
/// then flushes the store and commits the catalog durably, in that order.
/// Runs before anything else can use either.
pub(crate) fn reconcile(
    store: &BlockStore,
    catalog: &Catalog,
) -> Result<ReconcileReport, StorageError> {
    let mut report = ReconcileReport::default();
    let names: HashMap<u64, String> = catalog
        .layer_names()?
        .into_iter()
        .map(|(n, id)| (id, n))
        .collect();
    let mut known: HashSet<[u8; 17]> = HashSet::new();

    for (layer, path, guid) in catalog.all_layer_guids()? {
        let id = layer_file_id(&guid);
        known.insert(id);
        if store.stat(&id)?.is_some() {
            continue;
        }
        let lname = names
            .get(&layer)
            .cloned()
            .unwrap_or_else(|| format!("#{layer}"));
        tracing::warn!(
            layer = %lname, path = %path,
            "layer file data missing from the store (lost in a crash before a flush); \
             recreated empty"
        );
        store.set_len(&id, 0)?;
        if let Some(mut rec) = catalog.get(layer, &path)? {
            rec.len = 0;
            catalog.put(layer, &path, &rec, false)?;
        }
        report.emptied_files.push((lname, path));
    }

    for (h, _) in catalog.cache_all()? {
        let id = cache_file_id(&h);
        if store.stat(&id)?.is_some() {
            known.insert(id);
        } else {
            catalog.cache_remove(&h)?;
            report.cache_rows_dropped += 1;
        }
    }

    for id in store.file_ids()? {
        match classify_store_id(&id) {
            StoreIdKind::Foreign => {
                tracing::warn!(id = ?id, "store holds a file id vfs-storage never writes; left alone");
            }
            StoreIdKind::Layer(_) | StoreIdKind::Cache(_) => {
                let key: [u8; 17] = id.as_slice().try_into().expect("classified as 17 bytes");
                if known.contains(&key) {
                    continue;
                }
                match store.delete(&id) {
                    Ok(()) | Err(vfs_block_store::Error::NotFound) => {}
                    Err(e) => return Err(e.into()),
                }
                report.orphans_deleted += 1;
            }
        }
    }
    if report.cache_rows_dropped > 0 {
        tracing::warn!(
            rows = report.cache_rows_dropped,
            "cache rows without store data dropped"
        );
    }
    if report.orphans_deleted > 0 {
        tracing::warn!(
            files = report.orphans_deleted,
            "store files no catalog row references deleted"
        );
        store.compact(CompactOptions::default())?;
    }
    // Spec §6 order: the store's half first, then the catalog's.
    store.flush()?;
    catalog.commit_durable()?;
    Ok(report)
}

impl Storage {
    /// What reconciliation did when this `Storage` was opened.
    pub fn last_reconcile(&self) -> &ReconcileReport {
        &self.reconciled
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use vfs_provider::{Provider, VPath, KIND_FILE, OPEN_CREATE, OPEN_READ, OPEN_WRITE};

    use crate::catalog::{CacheRec, EntryRec};
    use crate::config::StorageConfig;
    use crate::ids::{cache_file_id, classify_store_id, layer_file_id, new_guid, StoreIdKind};
    use crate::storage::Storage;

    const BS: u64 = 4096;

    fn cfg() -> StorageConfig {
        let mut c = StorageConfig::default();
        c.store.block_size = BS as u32;
        c
    }

    fn at(p: &str) -> VPath<'_> {
        VPath::at_default(p)
    }

    fn write_file(p: &Arc<dyn Provider>, rel: &str, body: &[u8]) {
        let (h, _, _) = p.open(at(rel), OPEN_WRITE | OPEN_CREATE).unwrap();
        assert_eq!(p.write_at(h, 0, body).unwrap(), body.len());
        p.close(h).unwrap();
    }

    fn read_file(p: &Arc<dyn Provider>, rel: &str) -> Vec<u8> {
        let (h, size, _) = p.open(at(rel), OPEN_READ).unwrap();
        let mut out = vec![0u8; size as usize];
        let mut done = 0;
        while done < out.len() {
            let n = p.read_at(h, done as u64, &mut out[done..]).unwrap();
            if n == 0 {
                break;
            }
            done += n;
        }
        out.truncate(done);
        p.close(h).unwrap();
        out
    }

    /// Every store id a catalog row names, and every catalog file has store
    /// data: what reconciliation leaves behind.
    fn assert_consistent(s: &Storage) {
        let mut known = HashSet::new();
        for (_, path, g) in s.catalog.all_layer_guids().unwrap() {
            let id = layer_file_id(&g);
            assert!(
                s.store.stat(&id).unwrap().is_some(),
                "{path} lacks store data"
            );
            known.insert(id.to_vec());
        }
        for (h, _) in s.catalog.cache_all().unwrap() {
            let id = cache_file_id(&h);
            assert!(s.store.stat(&id).unwrap().is_some(), "cache row lacks data");
            known.insert(id.to_vec());
        }
        for id in s.store.file_ids().unwrap() {
            if classify_store_id(&id) != StoreIdKind::Foreign {
                assert!(known.contains(&id), "unreferenced store id {id:?}");
            }
        }
    }

    #[test]
    fn a_catalog_row_without_store_data_becomes_empty() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let lid = s.catalog.create_layer("l").unwrap();
        let guid = new_guid();
        let rec = EntryRec {
            name: "Lost.txt".into(),
            kind: KIND_FILE,
            guid,
            len: 5000,
            mtime: 7,
        };
        s.catalog.put(lid, "lost.txt", &rec, false).unwrap();
        s.close().unwrap();

        let s = Storage::open(d.path(), cfg()).unwrap();
        let r = s.last_reconcile();
        assert_eq!(
            r.emptied_files,
            vec![("l".to_string(), "lost.txt".to_string())]
        );
        assert_eq!(r.orphans_deleted, 0);
        assert_eq!(s.store.stat(&layer_file_id(&guid)).unwrap().unwrap().len, 0);
        let p = s.layer("l").unwrap();
        assert_eq!(p.getattr(at("lost.txt")).unwrap().unwrap().size, 0);
        assert_eq!(read_file(&p, "LOST.TXT"), b"");
        assert_consistent(&s);
        drop(p);
        s.close().unwrap();

        // The repair was durable: nothing left to do.
        let s = Storage::open(d.path(), cfg()).unwrap();
        assert_eq!(*s.last_reconcile(), Default::default());
    }

    #[test]
    fn store_orphans_are_deleted() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        // Referenced: a layer file and a cache file.
        let p = s.layer("l").unwrap();
        write_file(&p, "kept.bin", &vec![3u8; 3 * BS as usize]);
        drop(p);
        let h = [9u8; 16];
        s.store.set_len(&cache_file_id(&h), 10).unwrap();
        s.catalog
            .cache_put_many(&[(
                h,
                CacheRec {
                    last_access_min: 1,
                    logical_bytes: 0,
                },
            )])
            .unwrap();
        // Unreferenced: a layer id, a cache id, and a foreign id (left alone).
        let orphan_l = layer_file_id(&new_guid());
        let orphan_c = cache_file_id(&[1u8; 16]);
        s.store.set_len(&orphan_l, 10).unwrap();
        s.store.set_len(&orphan_c, 10).unwrap();
        s.store.set_len(b"foreign", 3).unwrap();
        s.close().unwrap();

        let s = Storage::open(d.path(), cfg()).unwrap();
        assert_eq!(s.last_reconcile().orphans_deleted, 2);
        assert!(s.last_reconcile().emptied_files.is_empty());
        let ids: HashSet<Vec<u8>> = s.store.file_ids().unwrap().into_iter().collect();
        assert!(!ids.contains(orphan_l.as_slice()));
        assert!(!ids.contains(orphan_c.as_slice()));
        assert!(ids.contains(cache_file_id(&h).as_slice()));
        assert!(ids.contains(b"foreign".as_slice()));
        assert_consistent(&s);
        let p = s.layer("l").unwrap();
        assert_eq!(read_file(&p, "kept.bin"), vec![3u8; 3 * BS as usize]);
    }

    #[test]
    fn cache_rows_without_store_data_are_dropped_from_the_budget() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let (lost, kept) = ([1u8; 16], [2u8; 16]);
        s.store.set_len(&cache_file_id(&kept), 300).unwrap();
        s.catalog
            .cache_put_many(&[
                (
                    lost,
                    CacheRec {
                        last_access_min: 1,
                        logical_bytes: 1000,
                    },
                ),
                (
                    kept,
                    CacheRec {
                        last_access_min: 1,
                        logical_bytes: 300,
                    },
                ),
            ])
            .unwrap();
        s.close().unwrap();

        let s = Storage::open(d.path(), cfg()).unwrap();
        assert_eq!(s.last_reconcile().cache_rows_dropped, 1);
        let rows: Vec<[u8; 16]> = s
            .catalog
            .cache_all()
            .unwrap()
            .into_iter()
            .map(|r| r.0)
            .collect();
        assert_eq!(rows, vec![kept]);
        assert_eq!(s.cache_stats().cached_logical_bytes, 300);
        assert_consistent(&s);
    }

    #[test]
    fn closed_file_survives_reopen_without_close() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("saves").unwrap();
        let body: Vec<u8> = (0..(5 * BS + 17)).map(|i| (i % 251) as u8).collect();
        write_file(&p, "Saves/quick.ess", &body);

        // Killed between the close and any later flush point.
        #[cfg(not(windows))]
        {
            let killed = tempfile::tempdir().unwrap();
            crate::test_util::snapshot(d.path(), killed.path());
            let k = Storage::open(killed.path(), cfg()).unwrap();
            assert_eq!(*k.last_reconcile(), Default::default());
            let kp = k.layer("saves").unwrap();
            assert_eq!(read_file(&kp, "saves/quick.ess"), body);
            assert_consistent(&k);
        }

        // Dropped without `Storage::close`.
        drop(p);
        drop(s);
        let s = Storage::open(d.path(), cfg()).unwrap();
        assert!(s.last_reconcile().emptied_files.is_empty());
        let p = s.layer("saves").unwrap();
        assert_eq!(read_file(&p, "Saves/Quick.ess"), body);
    }

    /// A kill while a file is being created and written (dirty blocks already
    /// committed to the store, nothing durable yet) leaves a store that opens
    /// consistent.
    #[cfg(not(windows))]
    #[test]
    fn a_kill_mid_write_reopens_consistent() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("l").unwrap();
        write_file(&p, "done.bin", b"done");
        let (h, _, _) = p
            .open(at("new/open.bin"), OPEN_WRITE | OPEN_CREATE)
            .unwrap();
        p.write_at(h, 0, &vec![7u8; 9 * BS as usize]).unwrap();
        s.store.flush().unwrap(); // the store half reached disk, the catalog did not

        let killed = tempfile::tempdir().unwrap();
        crate::test_util::snapshot(d.path(), killed.path());
        let k = Storage::open(killed.path(), cfg()).unwrap();
        assert_consistent(&k);
        assert!(k.last_reconcile().orphans_deleted >= 1);
        let kp = k.layer("l").unwrap();
        assert_eq!(read_file(&kp, "done.bin"), b"done");
        assert!(kp.getattr(at("new/open.bin")).unwrap().is_none());
        p.close(h).unwrap();
    }
}
