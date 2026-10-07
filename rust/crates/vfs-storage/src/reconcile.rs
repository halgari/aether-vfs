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
//! - a layer file with blocks missing at or past the block that holds its
//!   durable row's length (the store's own auto-flush or a compaction made a
//!   commit's `set_len` durable before the `write_blocks` that followed it,
//!   so the lost bytes were written by a handle that never closed — or, for
//!   the block holding the row's length, the tail a grow's resize dropped;
//!   see `FileCell::commit`): the missing blocks are written as zeros, so a
//!   layer file never has a missing block (spec §5);
//! - a layer file with blocks missing wholly below that block: bytes the
//!   durable row says a closed file holds are gone. That is corruption, and
//!   spec §5 never serves it as zeros: the blocks stay missing (a read of
//!   them is `ST_IO_ERROR`), the file is reported in
//!   [`ReconcileReport::corrupt_files`] and logged at error level, and its
//!   row keeps its length so the next open reports it again;
//! - a layer file row whose length differs from the store's (the row is
//!   updated after the blocks, and only a durable point publishes it): the
//!   row takes the store's length, which is what the data says;
//! - a `b'L'` or `b'C'` store id no catalog row names (a create whose row
//!   never became durable, or a delete that never reached the store): deleted;
//! - a cache row whose store file is missing: dropped, so the cache budget
//!   counts only what the store holds; and a cache row that counts no bytes
//!   (created by a fetch whose block write never landed): dropped, and its
//!   store file deleted as an orphan.
//!
//! Store ids of any other shape are not `vfs-storage`'s: they are logged and
//! left alone.
//!
//! None of this can follow a clean close ([`Storage::close`], or the drop of
//! the last reference) of a session that left nothing for this pass: every
//! write is then durable on both sides, in order, and the catalog and the
//! block store hold the same random clean-close token. A session that left a
//! repair here (a store delete that failed, a failed commit, a write that
//! panicked, corruption found: see `Storage::needs_reconcile`) leaves no
//! token. An open that finds matching tokens removes the catalog's durably
//! before anything else and skips this pass
//! ([`ReconcileReport::skipped_after_clean_close`]).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;
use std::sync::RwLock;

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
    /// Layer files that had blocks missing at or past the block holding
    /// their durable length, now filled with zeros: `(layer name, folded
    /// path)`.
    pub zero_filled_files: Vec<(String, String)>,
    /// Layer files with blocks missing below the block holding their durable
    /// length: closed data is gone. The blocks stay missing and read as
    /// `ST_IO_ERROR`: `(layer name, folded path)`.
    pub corrupt_files: Vec<(String, String)>,
    /// Layer file rows whose length disagreed with the store's, now set to
    /// the store's: `(layer name, folded path)`.
    pub resized_rows: Vec<(String, String)>,
    /// Store files no catalog row referenced, deleted.
    pub orphans_deleted: u64,
    /// Cache rows whose store file was missing, or that counted no bytes,
    /// dropped.
    pub cache_rows_dropped: u64,
    /// Repairs that failed and were skipped (logged at error level); the
    /// next open tries them again. Each is a one-line description.
    pub failed_repairs: Vec<String>,
    /// Reconciliation did not run: the storage was closed cleanly, so there
    /// was nothing to repair (every other field is then empty).
    pub skipped_after_clean_close: bool,
}

#[cfg(test)]
thread_local! {
    /// Test hook: every store repair (zero-fill, empty-file recreate,
    /// compaction) fails on this thread.
    pub(crate) static FAIL_REPAIRS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs the store repair `f`, with the test hook applied.
fn repair<T>(f: impl FnOnce() -> Result<T, StorageError>) -> Result<T, StorageError> {
    #[cfg(test)]
    if FAIL_REPAIRS.with(std::cell::Cell::get) {
        return Err(StorageError::Io(std::io::Error::other(
            "injected repair failure",
        )));
    }
    f()
}

/// Logs a failed repair at error level and records it in `report`.
fn repair_failed(report: &mut ReconcileReport, what: String, e: &StorageError) {
    tracing::error!(error = %e, "reconcile: {what} failed; left for the next open");
    report.failed_repairs.push(format!("{what}: {e}"));
}

/// Blocks per `write_blocks` call when zero-filling.
const RUN_BLOCKS: u64 = 64;

/// Brings `catalog` and `store` back into agreement (see the module docs),
/// then flushes the store and commits the catalog durably, in that order,
/// under the exclusive durability `gate`. Runs before anything else can use
/// either; `bs` is the store's block size.
///
/// Refuses (and changes nothing) when the catalog has never had a layer but
/// the store holds layer files: the catalog at `catalog_path` was lost or
/// replaced, and every layer file would otherwise look like an orphan and be
/// deleted. A new layer is made durable before any of its data is written
/// ([`Storage::layer`]), so this cannot follow a mere crash.
pub(crate) fn reconcile(
    store: &BlockStore,
    catalog: &Catalog,
    catalog_path: &Path,
    gate: &RwLock<()>,
    bs: u64,
) -> Result<ReconcileReport, StorageError> {
    let store_ids = store.file_ids()?;
    if !catalog.has_layer_history()?
        && store_ids
            .iter()
            .any(|id| matches!(classify_store_id(id), StoreIdKind::Layer(_)))
    {
        let dir = catalog_path.parent().unwrap_or(catalog_path);
        tracing::error!(
            catalog = %catalog_path.display(),
            "the catalog is missing or empty but the store holds layer data; refusing to open"
        );
        return Err(StorageError::Catalog(format!(
            "{} is missing (or empty) but the store in {} holds layer data; \
             restore the catalog or move the store aside",
            catalog_path.display(),
            dir.display()
        )));
    }
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
        let lname = names
            .get(&layer)
            .cloned()
            .unwrap_or_else(|| format!("#{layer}"));
        if let Some(info) = store.stat(&id)? {
            let row = catalog.get(layer, &path)?;
            let row_len = row.as_ref().map_or(0, |r| r.len);
            // Blocks wholly below the one holding the durable length held
            // closed bytes; from that block on, bytes a crash may lose.
            let boundary = row_len / bs * bs;
            let missing = missing_ranges(&store.cached_ranges(&id)?, info.len);
            let (lost, fill) = split_at(&missing, boundary);
            let corrupt = !lost.is_empty();
            if corrupt {
                tracing::error!(
                    layer = %lname, path = %path, ranges = ?lost, len = row_len,
                    "layer file has blocks missing below its durable length: corruption; \
                     left missing (reads of them fail)"
                );
                report.corrupt_files.push((lname.clone(), path.clone()));
            }
            if !fill.is_empty() {
                if fill[0].start < row_len {
                    tracing::error!(
                        layer = %lname, path = %path, ranges = ?fill, len = row_len,
                        "layer file lost the block holding its durable length (a crash \
                         between a resize and its block writes); filled with zeros"
                    );
                } else {
                    tracing::warn!(
                        layer = %lname, path = %path, ranges = ?fill,
                        "layer file has blocks missing past its durable length (a store \
                         flush landed between its resize and its block writes); filled \
                         with zeros"
                    );
                }
                let filled = fill
                    .iter()
                    .try_for_each(|r| repair(|| zero_fill(store, &id, r, info.len, bs)));
                match filled {
                    Ok(()) => report.zero_filled_files.push((lname.clone(), path.clone())),
                    Err(e) => {
                        repair_failed(&mut report, format!("zero-filling {lname}:{path}"), &e);
                        // Its row keeps its length, for the same reason as a
                        // corrupt file's below.
                        continue;
                    }
                }
            }
            // A corrupt file keeps its row's length: taking the store's could
            // move the boundary below the hole and zero-fill it next time.
            if let Some(mut rec) = row.filter(|_| !corrupt) {
                if rec.len != info.len {
                    tracing::warn!(
                        layer = %lname, path = %path, row = rec.len, store = info.len,
                        "layer file row length differs from the store's; using the store's"
                    );
                    rec.len = info.len;
                    match catalog.put(layer, &path, &rec) {
                        Ok(()) => report.resized_rows.push((lname, path)),
                        Err(e) => repair_failed(
                            &mut report,
                            format!("correcting the row length of {lname}:{path}"),
                            &e,
                        ),
                    }
                }
            }
            continue;
        }
        tracing::warn!(
            layer = %lname, path = %path,
            "layer file data missing from the store (lost in a crash before a flush); \
             recreated empty"
        );
        let emptied = repair(|| Ok(store.set_len(&id, 0)?)).and_then(|()| {
            if let Some(mut rec) = catalog.get(layer, &path)? {
                rec.len = 0;
                catalog.put(layer, &path, &rec)?;
            }
            Ok(())
        });
        match emptied {
            Ok(()) => report.emptied_files.push((lname, path)),
            Err(e) => repair_failed(&mut report, format!("recreating {lname}:{path} empty"), &e),
        }
    }

    for (h, rec) in catalog.cache_all()? {
        let id = cache_file_id(&h);
        if rec.logical_bytes > 0 && store.stat(&id)?.is_some() {
            known.insert(id);
        } else {
            catalog.cache_remove(&h)?;
            report.cache_rows_dropped += 1;
        }
    }

    for id in store_ids {
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
                    Ok(()) | Err(vfs_block_store::Error::NotFound) => report.orphans_deleted += 1,
                    // Harmless to keep: the next open tries again.
                    Err(e) => {
                        tracing::warn!(id = ?id, error = %e, "deleting an orphan store file failed")
                    }
                }
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
        if let Err(e) = repair(|| Ok(store.compact(CompactOptions::default())?)) {
            repair_failed(
                &mut report,
                "compacting after the orphan deletes".into(),
                &e,
            );
        }
    }
    // Spec §6 order: the store's half first, then the catalog's.
    let _gate = gate.write().unwrap_or_else(|e| e.into_inner());
    store.flush()?;
    catalog.commit_durable()?;
    Ok(report)
}

/// The parts of `0..len` that `cached`, the store's merged and ordered
/// stored ranges of one file, does not cover.
fn missing_ranges(cached: &[Range<u64>], len: u64) -> Vec<Range<u64>> {
    let mut out = Vec::new();
    let mut at = 0u64;
    for r in cached {
        if r.start > at {
            out.push(at..r.start.min(len));
        }
        at = at.max(r.end);
    }
    if at < len {
        out.push(at..len);
    }
    out.retain(|r| r.start < r.end);
    out
}

/// Splits block-aligned `ranges` at `at` (a block boundary): the parts below
/// it and the parts at or past it.
fn split_at(ranges: &[Range<u64>], at: u64) -> (Vec<Range<u64>>, Vec<Range<u64>>) {
    let mut below = Vec::new();
    let mut above = Vec::new();
    for r in ranges {
        if r.start < at {
            below.push(r.start..r.end.min(at));
        }
        if r.end > at {
            above.push(r.start.max(at)..r.end);
        }
    }
    (below, above)
}

/// Writes zero blocks over the missing range `r` of a file of length `len`.
/// The store stores whole blocks, so `r` starts on a block boundary and ends
/// on one or at `len`; should it ever start inside a block, that block is
/// stored and is left alone.
fn zero_fill(
    store: &BlockStore,
    id: &[u8],
    r: &Range<u64>,
    len: u64,
    bs: u64,
) -> Result<(), StorageError> {
    let first = r.start.div_ceil(bs);
    let end = r.end.div_ceil(bs);
    let mut b = first;
    while b < end {
        let n = (end - b).min(RUN_BLOCKS);
        let bytes = ((b + n) * bs).min(len) - b * bs;
        store.write_blocks(id, b, &vec![0u8; bytes as usize])?;
        b += n;
    }
    Ok(())
}

impl Storage {
    /// What reconciliation did when this `Storage` was opened, or that it was
    /// skipped after a clean close.
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
    use crate::config::{Durability, StorageConfig};
    use crate::ids::{cache_file_id, classify_store_id, layer_file_id, new_guid, StoreIdKind};
    use crate::storage::Storage;

    const BS: u64 = 4096;

    fn cfg() -> StorageConfig {
        let mut c = StorageConfig::default();
        c.store.block_size = BS as u32;
        c
    }

    /// [`cfg`] with every close, flush and namespace change a durable point.
    fn cfg_every_close() -> StorageConfig {
        StorageConfig {
            durability: Durability::OnEveryClose,
            ..cfg()
        }
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
        s.catalog.put(lid, "lost.txt", &rec).unwrap();
        s.close_unclean(); // a crash: no clean-close mark

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
        s.close_unclean(); // a crash: no clean-close mark

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
                    logical_bytes: 10,
                },
            )])
            .unwrap();
        // Unreferenced: a layer id, a cache id, and a foreign id (left alone).
        let orphan_l = layer_file_id(&new_guid());
        let orphan_c = cache_file_id(&[1u8; 16]);
        s.store.set_len(&orphan_l, 10).unwrap();
        s.store.set_len(&orphan_c, 10).unwrap();
        s.store.set_len(b"foreign", 3).unwrap();
        s.close_unclean(); // a crash: no clean-close mark

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
        s.close_unclean(); // a crash: no clean-close mark

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

    /// A cache row that counts no bytes (its fetch's block write never
    /// landed) is dropped with its store file.
    #[test]
    fn zero_byte_cache_rows_are_dropped_with_their_files() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let h = [5u8; 16];
        s.catalog
            .cache_put_many(&[(
                h,
                CacheRec {
                    last_access_min: 1,
                    logical_bytes: 0,
                },
            )])
            .unwrap();
        s.store.set_len(&cache_file_id(&h), 4000).unwrap();
        s.close_unclean(); // a crash: no clean-close mark

        let s = Storage::open(d.path(), cfg()).unwrap();
        assert_eq!(s.last_reconcile().cache_rows_dropped, 1);
        assert_eq!(s.last_reconcile().orphans_deleted, 1);
        assert!(s.catalog.cache_all().unwrap().is_empty());
        assert!(s.store.file_ids().unwrap().is_empty());
    }

    #[test]
    fn closed_file_survives_reopen_without_close() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg_every_close()).unwrap();
        let p = s.layer("saves").unwrap();
        let body: Vec<u8> = (0..(5 * BS + 17)).map(|i| (i % 251) as u8).collect();
        write_file(&p, "Saves/quick.ess", &body);

        // Killed between the close and any later flush point.
        #[cfg(not(windows))]
        {
            let killed = tempfile::tempdir().unwrap();
            crate::test_util::snapshot_as_killed(d.path(), killed.path());
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
        s.sync().unwrap(); // done.bin is durable, whatever the policy
        let (h, _, _) = p
            .open(at("new/open.bin"), OPEN_WRITE | OPEN_CREATE)
            .unwrap();
        p.write_at(h, 0, &vec![7u8; 9 * BS as usize]).unwrap();
        s.store.flush().unwrap(); // the store half reached disk, the catalog did not

        let killed = tempfile::tempdir().unwrap();
        crate::test_util::snapshot_as_killed(d.path(), killed.path());
        let k = Storage::open(killed.path(), cfg()).unwrap();
        assert_consistent(&k);
        assert!(k.last_reconcile().orphans_deleted >= 1);
        let kp = k.layer("l").unwrap();
        assert_eq!(read_file(&kp, "done.bin"), b"done");
        assert!(kp.getattr(at("new/open.bin")).unwrap().is_none());
        p.close(h).unwrap();
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn missing_ranges_are_the_complement_below_the_length() {
        use super::missing_ranges;
        assert_eq!(missing_ranges(&[], 10), vec![0..10]);
        assert_eq!(
            missing_ranges(&[0..10], 10),
            Vec::<std::ops::Range<u64>>::new()
        );
        assert_eq!(
            missing_ranges(&[4..8, 12..16], 20),
            vec![0..4, 8..12, 16..20]
        );
        assert_eq!(
            missing_ranges(&[0..4], 0),
            Vec::<std::ops::Range<u64>>::new()
        );
    }

    /// The store's own auto-flush (or a compaction) made a grow durable before
    /// the zero blocks that follow it in a commit; the durable row still has
    /// the old length. Every missing block is at or past the block holding
    /// that length, so reopening fills them with zeros — including that
    /// block, the closed tail the grow's resize dropped (the residual crash
    /// window `FileCell::commit` documents).
    #[test]
    fn missing_blocks_from_the_durable_length_on_are_zero_filled() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("l").unwrap();
        let body = vec![0x5Au8; 2 * BS as usize + 50];
        write_file(&p, "Grown.bin", &body);
        drop(p);
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        let rec = s.catalog.get(lid, "grown.bin").unwrap().unwrap();
        let id = layer_file_id(&rec.guid);
        let len = 5 * BS + 100;
        s.store.set_len(&id, len).unwrap(); // blocks 2..6 now missing
        s.close_unclean(); // a crash: no clean-close mark

        let s = Storage::open(d.path(), cfg()).unwrap();
        let r = s.last_reconcile();
        assert_eq!(
            r.zero_filled_files,
            vec![("l".to_string(), "grown.bin".to_string())]
        );
        assert!(r.corrupt_files.is_empty(), "{r:?}");
        assert_eq!(
            r.resized_rows,
            vec![("l".to_string(), "grown.bin".to_string())]
        );
        assert_eq!(s.store.cached_ranges(&id).unwrap(), vec![0..len]);
        let p = s.layer("l").unwrap();
        assert_eq!(p.getattr(at("grown.bin")).unwrap().unwrap().size, len);
        let mut want = body[..2 * BS as usize].to_vec();
        want.resize(len as usize, 0);
        assert_eq!(read_file(&p, "grown.bin"), want);
        assert_consistent(&s);
    }

    /// Blocks missing wholly below the block holding the durable length are
    /// closed data that is gone: never served as zeros (spec §5). They stay
    /// missing, reads of them fail, the file is reported, and the next open
    /// reports it again rather than filling it.
    #[test]
    fn missing_blocks_below_the_durable_length_are_corruption() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("l").unwrap();
        let len = 5 * BS + 100;
        let body: Vec<u8> = (0..len).map(|i| (i % 241) as u8).collect();
        write_file(&p, "Save.ess", &body);
        drop(p);
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        let id = layer_file_id(&s.catalog.get(lid, "save.ess").unwrap().unwrap().guid);
        // Lose blocks 1 and 2: cut to one block, grow back, restore 3..=5.
        s.store.set_len(&id, BS).unwrap();
        s.store.set_len(&id, len).unwrap();
        s.store
            .write_blocks(&id, 3, &body[3 * BS as usize..])
            .unwrap();
        s.close_unclean(); // a crash: no clean-close mark

        for round in 0..2 {
            let s = Storage::open(d.path(), cfg()).unwrap();
            let r = s.last_reconcile().clone();
            assert_eq!(
                r.corrupt_files,
                vec![("l".to_string(), "save.ess".to_string())],
                "round {round}"
            );
            assert!(r.zero_filled_files.is_empty(), "round {round}: {r:?}");
            assert_eq!(
                s.store.cached_ranges(&id).unwrap(),
                vec![0..BS, 3 * BS..len],
                "round {round}: left missing"
            );
            let p = s.layer("l").unwrap();
            let (h, size, _) = p.open(at("save.ess"), OPEN_READ).unwrap();
            assert_eq!(size, len);
            let mut buf = vec![0u8; BS as usize];
            assert_eq!(p.read_at(h, 0, &mut buf).unwrap(), BS as usize);
            assert_eq!(buf, body[..BS as usize]);
            assert_eq!(
                p.read_at(h, BS + 10, &mut buf),
                Err(vfs_provider::ST_IO_ERROR),
                "round {round}: a lost block is an error, never zeros"
            );
            assert_eq!(p.read_at(h, 3 * BS, &mut buf).unwrap(), BS as usize);
            assert_eq!(buf, body[3 * BS as usize..4 * BS as usize]);
            p.close(h).unwrap();
            drop(p);
            s.close().unwrap();
        }
    }

    /// A shrink's resize made durable without its tail write (the store is
    /// shorter than the durable row, its new tail block missing): corruption,
    /// and the row keeps its length, so a second open does not move the
    /// boundary below the hole and fill it with zeros.
    #[test]
    fn a_lost_tail_below_a_longer_row_stays_reported() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("l").unwrap();
        let len = 5 * BS + 100;
        write_file(&p, "f.bin", &vec![7u8; len as usize]);
        drop(p);
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        let id = layer_file_id(&s.catalog.get(lid, "f.bin").unwrap().unwrap().guid);
        s.store.set_len(&id, 2 * BS + 7).unwrap(); // block 2 dropped
        s.close_unclean(); // a crash: no clean-close mark

        for round in 0..2 {
            let s = Storage::open(d.path(), cfg()).unwrap();
            let r = s.last_reconcile().clone();
            assert_eq!(
                r.corrupt_files,
                vec![("l".to_string(), "f.bin".to_string())],
                "round {round}"
            );
            assert!(
                r.zero_filled_files.is_empty() && r.resized_rows.is_empty(),
                "{r:?}"
            );
            assert_eq!(s.catalog.get(lid, "f.bin").unwrap().unwrap().len, len);
            s.close().unwrap();
        }
    }

    /// A catalog that is gone (or was replaced by an empty one) while the
    /// store holds layer data: opening refuses, naming the directory, and
    /// deletes nothing. Restoring the catalog brings everything back.
    #[test]
    fn a_missing_catalog_with_layer_data_refuses_to_open() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("saves").unwrap();
        write_file(&p, "a.ess", b"precious");
        drop(p);
        s.close().unwrap();
        let cat = d.path().join("catalog.redb");
        let backup = d.path().join("catalog.bak");
        std::fs::rename(&cat, &backup).unwrap();

        for attempt in 0..2 {
            let e = Storage::open(d.path(), cfg())
                .err()
                .expect("must refuse to open");
            let msg = e.to_string();
            assert!(
                msg.contains("catalog.redb") && msg.contains(&d.path().display().to_string()),
                "attempt {attempt}: {msg}"
            );
        }
        std::fs::remove_file(&cat).ok();
        std::fs::rename(&backup, &cat).unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        assert_eq!(*s.last_reconcile(), Default::default());
        let p = s.layer("saves").unwrap();
        assert_eq!(read_file(&p, "a.ess"), b"precious");
    }

    /// With no catalog and only cache files in the store, those are orphans
    /// like any other and are deleted.
    #[test]
    fn a_missing_catalog_with_only_cache_data_opens() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        s.store.set_len(&cache_file_id(&[4u8; 16]), 10).unwrap();
        s.close().unwrap();
        std::fs::remove_file(d.path().join("catalog.redb")).unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        assert_eq!(s.last_reconcile().orphans_deleted, 1);
        assert_consistent(&s);
    }

    /// A new layer is durable as soon as it exists, before any of its data:
    /// otherwise a crash could leave layer data in the store under a catalog
    /// that never had a layer, which opening refuses.
    #[cfg(not(windows))]
    #[test]
    fn a_new_layer_is_durable_before_its_data() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("fresh").unwrap();
        let killed = tempfile::tempdir().unwrap();
        crate::test_util::snapshot_as_killed(d.path(), killed.path());
        let k = Storage::open(killed.path(), cfg()).unwrap();
        assert!(k.catalog.layer_id("fresh").unwrap().is_some());
        drop(p);
    }

    /// A repair that fails (a zero-fill, recreating a lost file, the
    /// compaction) is logged, reported and skipped: the store still opens,
    /// and the next open repairs it.
    #[test]
    fn a_failed_repair_does_not_stop_the_store_opening() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("l").unwrap();
        write_file(&p, "grown.bin", &vec![1u8; 2 * BS as usize]);
        write_file(&p, "ok.bin", b"fine");
        drop(p);
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        let grown = layer_file_id(&s.catalog.get(lid, "grown.bin").unwrap().unwrap().guid);
        s.store.set_len(&grown, 4 * BS).unwrap(); // blocks 2, 3 to zero-fill
        let lost = EntryRec {
            name: "lost.bin".into(),
            kind: KIND_FILE,
            guid: new_guid(),
            len: 10,
            mtime: 1,
        };
        s.catalog.put(lid, "lost.bin", &lost).unwrap(); // to recreate empty
        s.store.set_len(&layer_file_id(&new_guid()), 5).unwrap(); // an orphan: compaction
        s.close_unclean(); // a crash: no clean-close mark

        super::FAIL_REPAIRS.with(|f| f.set(true));
        let s = Storage::open(d.path(), cfg());
        super::FAIL_REPAIRS.with(|f| f.set(false));
        let s = s.expect("a failed repair must not stop the open");
        let r = s.last_reconcile().clone();
        assert_eq!(r.failed_repairs.len(), 3, "{r:?}");
        assert!(
            r.zero_filled_files.is_empty() && r.emptied_files.is_empty(),
            "{r:?}"
        );
        assert!(
            r.resized_rows.is_empty(),
            "an unfilled file keeps its row: {r:?}"
        );
        let p = s.layer("l").unwrap();
        assert_eq!(read_file(&p, "ok.bin"), b"fine");
        drop(p);
        s.close().unwrap();

        let s = Storage::open(d.path(), cfg()).unwrap();
        let r = s.last_reconcile();
        assert!(r.failed_repairs.is_empty(), "{r:?}");
        assert_eq!(
            r.zero_filled_files,
            vec![("l".to_string(), "grown.bin".to_string())]
        );
        assert_eq!(
            r.emptied_files,
            vec![("l".to_string(), "lost.bin".to_string())]
        );
        assert_consistent(&s);
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn split_at_cuts_ranges_at_a_block_boundary() {
        use super::split_at;
        assert_eq!(
            split_at(&[0..4, 8..16, 20..22], 12),
            (vec![0..4, 8..12], vec![12..16, 20..22])
        );
        assert_eq!(split_at(&[8..16], 0), (vec![], vec![8..16]));
        assert_eq!(split_at(&[8..16], 16), (vec![8..16], vec![]));
    }

    /// A row whose length disagrees with the store's (the store's data made
    /// durable, the row update not) takes the store's length.
    #[test]
    fn a_row_length_is_set_to_the_stores() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("l").unwrap();
        let body: Vec<u8> = (0..(2 * BS + 9)).map(|i| (i % 7) as u8).collect();
        write_file(&p, "f.bin", &body);
        drop(p);
        let lid = s.catalog.layer_id("l").unwrap().unwrap();
        let mut rec = s.catalog.get(lid, "f.bin").unwrap().unwrap();
        rec.len = 100;
        s.catalog.put(lid, "f.bin", &rec).unwrap();
        s.close_unclean(); // a crash: no clean-close mark

        let s = Storage::open(d.path(), cfg()).unwrap();
        let r = s.last_reconcile();
        assert_eq!(r.resized_rows, vec![("l".to_string(), "f.bin".to_string())]);
        assert!(r.zero_filled_files.is_empty());
        assert_eq!(
            s.catalog.get(lid, "f.bin").unwrap().unwrap().len,
            body.len() as u64
        );
        let p = s.layer("l").unwrap();
        assert_eq!(
            p.getattr(at("f.bin")).unwrap().unwrap().size,
            body.len() as u64
        );
        assert_eq!(read_file(&p, "f.bin"), body);
    }

    /// Deferred: `sync`, and the flush of a rewrite of a durable file (which
    /// makes a durable point at once), wait for a commit that holds the
    /// durability gate, as under `OnEveryClose`.
    #[test]
    fn a_deferred_durable_point_waits_for_in_flight_commits() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg()).unwrap();
        let p = s.layer("l").unwrap();
        write_file(&p, "f", b"old");
        s.sync().unwrap();
        let (h, _, _) = p.open(at("f"), OPEN_WRITE).unwrap();
        p.write_at(h, 0, b"abc").unwrap();
        for via_flush in [false, true] {
            let in_flight = s.gate_shared(); // another layer's commit, mid-way
            let (tx, rx) = std::sync::mpsc::channel();
            let (s2, p2) = (Arc::clone(&s), Arc::clone(&p));
            let t = std::thread::spawn(move || {
                if via_flush {
                    p2.flush(h).unwrap();
                } else {
                    p2.write_at(h, 3, b"d").unwrap();
                    s2.sync().unwrap();
                }
                tx.send(()).unwrap();
            });
            assert!(
                rx.recv_timeout(std::time::Duration::from_millis(300))
                    .is_err(),
                "the durable point ran while a commit held the gate (flush: {via_flush})"
            );
            drop(in_flight);
            rx.recv().unwrap();
            t.join().unwrap();
        }
        p.close(h).unwrap();
    }

    /// A durable point waits for a commit that holds the durability gate, so
    /// no row can land between its store flush and its catalog commit.
    #[test]
    fn a_durable_point_waits_for_in_flight_commits() {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(d.path(), cfg_every_close()).unwrap();
        let p = s.layer("l").unwrap();
        let (h, _, _) = p.open(at("f"), OPEN_WRITE | OPEN_CREATE).unwrap();
        p.write_at(h, 0, b"abc").unwrap();
        let in_flight = s.gate_shared(); // another layer's commit, mid-way
        let (tx, rx) = std::sync::mpsc::channel();
        let p2 = Arc::clone(&p);
        let t = std::thread::spawn(move || {
            p2.flush(h).unwrap();
            tx.send(()).unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "the durable point ran while a commit held the gate"
        );
        drop(in_flight);
        rx.recv().unwrap();
        t.join().unwrap();
        p.close(h).unwrap();
    }
}
