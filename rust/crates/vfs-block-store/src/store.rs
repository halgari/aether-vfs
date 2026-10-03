//! `BlockStore`: opening, recovery, flushing and file-level metadata operations.

use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::compress::Codec;
use crate::config::StoreConfig;
use crate::error::{Error, Result};
use crate::index::{
    Index, META_BLOCK_SIZE, META_CLEAN_SHUTDOWN, META_NEXT_PACK_ID, META_SCHEMA_VERSION, PackInfo,
    PackState, SCHEMA_VERSION, Tables,
};
use crate::manifest::{MISSING, block_count, decode_ids, file_len, len_fits, segment_count};
use crate::pack::{PackFiles, PackWriter, list_pack_ids, remove_pack_file, sync_dir};
use crate::tracker::ReadTracker;
use crate::{crash, files};

/// Metadata of a stored file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileInfo {
    pub len: u64,
}

/// Result of [`BlockStore::read`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResult {
    /// Bytes covered by the read after clamping to end of file.
    pub bytes: usize,
    /// Byte ranges (whole blocks, clamped to end of file) that are not cached. The matching
    /// bytes of the caller's buffer are left unspecified.
    pub missing: Vec<Range<u64>>,
}

pub(crate) struct Writer {
    pub packs: PackWriter,
    pub next_pack_id: u32,
}

/// A deduplicating, compressing block store. Cheap to share between threads (`Send + Sync`).
pub struct BlockStore {
    pub(crate) cfg: StoreConfig,
    pub(crate) pack_dir: PathBuf,
    pub(crate) index: Index,
    pub(crate) packs: PackFiles,
    pub(crate) writer: Mutex<Writer>,
    pub(crate) tracker: ReadTracker,
    /// Retired packs waiting for older reads to finish: (pack id, generation).
    pub(crate) retired: Mutex<Vec<(u32, u64)>>,
    pub(crate) compact_lock: Mutex<()>,
    pub(crate) pool: Option<rayon::ThreadPool>,
    /// Compresses new blocks per write class, and counts what writes stored.
    pub(crate) codec: Codec,
    pub(crate) unflushed: AtomicU64,
    /// Non-durable index commits since the last durable commit.
    pub(crate) unflushed_commits: AtomicU64,
    pub(crate) healed: AtomicU64,
    shut_down: AtomicBool,
    _lock: File,
    #[cfg(test)]
    pub(crate) hooks: TestHooks,
}

/// Failure and race injection for unit tests.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestHooks {
    /// Runs once in `write_chunk`, after the records are appended and before the commit.
    #[allow(clippy::type_complexity)]
    pub before_write_commit: Mutex<Option<Box<dyn FnOnce(&BlockStore) + Send>>>,
    /// Makes the next retired-pack row removal in `delete_retired` fail.
    pub fail_retired_row_removal: AtomicBool,
}

impl BlockStore {
    /// Opens or creates a store in directory `dir`.
    pub fn open(dir: impl AsRef<Path>, cfg: StoreConfig) -> Result<Self> {
        Self::open_with(dir, cfg, Codec::new)
    }

    /// [`BlockStore::open`] with a stand-in for the GPU.
    #[cfg(all(test, feature = "gpu-zstd"))]
    pub(crate) fn open_with_engine(
        dir: impl AsRef<Path>,
        cfg: StoreConfig,
        factory: crate::gpu::EngineFactory,
    ) -> Result<Self> {
        Self::open_with(dir, cfg, move |c| Codec::with_factory(c, factory))
    }

    fn open_with(
        dir: impl AsRef<Path>,
        cfg: StoreConfig,
        codec: impl FnOnce(&StoreConfig) -> Codec,
    ) -> Result<Self> {
        cfg.validate()?;
        let dir = dir.as_ref();
        let pack_dir = dir.join("packs");
        std::fs::create_dir_all(&pack_dir)?;
        // Make the `packs` directory entry durable (a no-op on Windows).
        sync_dir(dir)?;

        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("LOCK"))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(Error::Locked),
            Err(TryLockError::Error(e)) => return Err(e.into()),
        }

        let index = Index::open(&dir.join("index.redb"), cfg.index_cache_bytes)?;
        let (resume, next_pack_id) = index.update(true, |t| recover(t, &pack_dir, &cfg))?;

        let mut packs = PackWriter::new(pack_dir.clone(), cfg.max_pack_size);
        if let Some(id) = resume {
            packs.resume(id)?;
        }
        let pool = cfg
            .compression_threads
            .map(|n| rayon::ThreadPoolBuilder::new().num_threads(n).build())
            .transpose()
            .map_err(|e| Error::Config(e.to_string()))?;

        Ok(Self {
            packs: PackFiles::new(pack_dir.clone()),
            writer: Mutex::new(Writer {
                packs,
                next_pack_id,
            }),
            pack_dir,
            index,
            tracker: ReadTracker::default(),
            retired: Mutex::new(Vec::new()),
            compact_lock: Mutex::new(()),
            pool,
            codec: codec(&cfg),
            unflushed: AtomicU64::new(0),
            unflushed_commits: AtomicU64::new(0),
            healed: AtomicU64::new(0),
            shut_down: AtomicBool::new(false),
            _lock: lock,
            cfg,
            #[cfg(test)]
            hooks: TestHooks::default(),
        })
    }

    /// Flushes and marks the store cleanly closed. Dropping the store does the same, but ignores errors.
    pub fn close(self) -> Result<()> {
        self.shutdown()
    }

    fn shutdown(&self) -> Result<()> {
        if self.shut_down.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // No write can be running (close takes the store; drop has it alone), but stop the GPU
        // service before the last durable commit all the same.
        self.codec.shutdown();
        self.durable_commit(|t| t.put_meta(META_CLEAN_SHUTDOWN, 1))
    }

    /// Makes every write that completed before this call durable.
    pub fn flush(&self) -> Result<()> {
        self.durable_commit(|_| Ok(()))
    }

    /// Whether anything was written since the last durable flush: records
    /// appended, or index changes (`set_len`, `delete`, block writes)
    /// committed non-durably. When false, [`BlockStore::flush`] has nothing
    /// to make durable.
    pub fn has_unflushed(&self) -> bool {
        self.unflushed.load(Ordering::Relaxed) > 0
            || self.unflushed_commits.load(Ordering::Relaxed) > 0
    }

    /// Syncs pack data, then commits `f` with `Durability::Immediate`. The writer lock is held
    /// throughout, so no record can be appended between the sync and the durable commit.
    pub(crate) fn durable_commit<R>(
        &self,
        f: impl FnOnce(&mut Tables<'_>) -> Result<R>,
    ) -> Result<R> {
        let w = self.writer.lock().unwrap();
        self.durable_commit_locked(w, f)
    }

    /// [`BlockStore::durable_commit`] with the writer lock already taken.
    fn durable_commit_locked<R>(
        &self,
        mut w: std::sync::MutexGuard<'_, Writer>,
        f: impl FnOnce(&mut Tables<'_>) -> Result<R>,
    ) -> Result<R> {
        w.packs.sync()?;
        crash::point("flush_before_commit");
        // Commits counted after this load may or may not be covered; counting them again is safe.
        let commits = self.unflushed_commits.load(Ordering::Relaxed);
        let r = self.index.update(true, f)?;
        self.unflushed.store(0, Ordering::Relaxed);
        self.unflushed_commits.fetch_sub(commits, Ordering::Relaxed);
        drop(w);
        Ok(r)
    }

    /// Runs `f` in a non-durable index transaction and counts the commit toward auto-flush.
    /// Every non-durable commit the store makes goes through here.
    pub(crate) fn commit<R>(&self, f: impl FnOnce(&mut Tables<'_>) -> Result<R>) -> Result<R> {
        let r = self.index.update(false, f)?;
        self.unflushed_commits.fetch_add(1, Ordering::Relaxed);
        Ok(r)
    }

    /// Whether `auto_flush_bytes` bytes or `auto_flush_commits` non-durable commits have piled
    /// up since the last durable flush.
    fn auto_flush_due(&self) -> bool {
        self.unflushed.load(Ordering::Relaxed) >= self.cfg.auto_flush_bytes
            || self.unflushed_commits.load(Ordering::Relaxed) >= self.cfg.auto_flush_commits
    }

    /// Flushes once `auto_flush_bytes` bytes or `auto_flush_commits` non-durable commits have
    /// piled up, since redb holds memory for non-durable commits until the next durable one.
    ///
    /// Every writer that crosses the threshold gets here at about the same time; the condition
    /// is checked again under the writer lock, so only the first of them flushes and the rest
    /// find nothing due (before, each ran a durable flush of its own: dozens back to back).
    pub(crate) fn maybe_auto_flush(&self) -> Result<()> {
        if !self.auto_flush_due() {
            return Ok(());
        }
        let w = self.writer.lock().unwrap();
        if !self.auto_flush_due() {
            return Ok(());
        }
        self.durable_commit_locked(w, |_| Ok(()))
    }

    /// Appends (header, payload) records to the active pack, starting new packs as needed.
    /// Returns (pack id, offset) per record. Records are visible to readers when this returns.
    /// This is the only place the writer lock is held while calling `Index::update`; `update`
    /// callbacks never take the writer lock, so this cannot deadlock.
    pub(crate) fn append_records<'a>(
        &self,
        records: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
    ) -> Result<Vec<(u32, u64)>> {
        let mut w = self.writer.lock().unwrap();
        let mut out = Vec::new();
        let mut bytes = 0u64;
        for (header, payload) in records {
            let len = (header.len() + payload.len()) as u64;
            if w.packs.needs_new_pack(len) {
                let id = w.next_pack_id;
                w.next_pack_id += 1;
                let old = w.packs.current_id();
                // Register before creating the file: an orphan row is cleaned up on open, and no
                // commit can reference a pack before its registration commit.
                self.commit(|t| {
                    if let Some(old) = old
                        && let Some(mut info) = t.pack(old)?
                    {
                        info.state = PackState::Sealed;
                        t.put_pack(old, &info)?;
                    }
                    t.put_pack(
                        id,
                        &PackInfo {
                            live_bytes: 0,
                            state: PackState::Active,
                        },
                    )?;
                    t.put_meta(META_NEXT_PACK_ID, id as u64 + 1)
                })?;
                w.packs.start_pack(id)?;
            }
            out.push(w.packs.append(header, payload)?);
            bytes += len;
        }
        w.packs.flush_buffer()?;
        self.unflushed.fetch_add(bytes, Ordering::Relaxed);
        Ok(out)
    }

    pub(crate) fn check_id(&self, file_id: &[u8]) -> Result<()> {
        if file_id.len() > self.cfg.max_file_id_len {
            return Err(Error::FileIdTooLong);
        }
        Ok(())
    }

    pub(crate) fn install<R: Send>(&self, f: impl FnOnce() -> R + Send) -> R {
        match &self.pool {
            Some(pool) => pool.install(f),
            None => f(),
        }
    }

    /// File metadata, or `None` if the file does not exist. One index lookup.
    pub fn stat(&self, file_id: &[u8]) -> Result<Option<FileInfo>> {
        self.check_id(file_id)?;
        let r = self.index.read()?;
        Ok(r.segment(file_id, 0)?
            .map(|s| FileInfo { len: file_len(&s) }))
    }

    /// Every stored file id, once each, in key order. For callers that keep
    /// their own catalog and must reconcile it with the store after a crash.
    pub fn file_ids(&self) -> Result<Vec<Vec<u8>>> {
        let _guard = self.tracker.enter();
        let r = self.index.read()?;
        let mut out = Vec::new();
        r.for_each_segment(|id, seg, _| {
            if seg == 0 {
                out.push(id.to_vec());
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// Creates the file if it does not exist (every block missing), or changes its length.
    /// Blocks past the new end, and a last block whose length changes, become missing.
    pub fn set_len(&self, file_id: &[u8], len: u64) -> Result<()> {
        self.check_id(file_id)?;
        let bs = self.cfg.block_size;
        if !len_fits(len, bs) {
            return Err(Error::OutOfRange);
        }
        self.commit(|t| {
            for id in files::resize(t, file_id, len, bs)? {
                t.decref(id)?;
            }
            Ok(())
        })?;
        self.maybe_auto_flush()
    }

    /// Deletes a file. Its blocks are freed once no other file references them.
    pub fn delete(&self, file_id: &[u8]) -> Result<()> {
        self.check_id(file_id)?;
        let bs = self.cfg.block_size;
        self.commit(|t| {
            for id in files::remove(t, file_id, bs)?.ok_or(Error::NotFound)? {
                t.decref(id)?;
            }
            Ok(())
        })?;
        self.maybe_auto_flush()
    }

    /// Cached byte ranges of a file, merged and in order.
    pub fn cached_ranges(&self, file_id: &[u8]) -> Result<Vec<Range<u64>>> {
        self.check_id(file_id)?;
        let bs = self.cfg.block_size as u64;
        let r = self.index.read()?;
        let len = file_len(&r.segment(file_id, 0)?.ok_or(Error::NotFound)?);
        let mut out: Vec<Range<u64>> = Vec::new();
        for s in 0..segment_count(block_count(len, bs as u32)) {
            let value = r
                .segment(file_id, s)?
                .ok_or_else(|| Error::Corrupt(format!("manifest segment {s} missing")))?;
            for (i, id) in decode_ids(&value, s).into_iter().enumerate() {
                if id == MISSING || r.block(id)?.is_none() {
                    continue;
                }
                let start = (s as u64 * crate::manifest::BLOCKS_PER_SEGMENT + i as u64) * bs;
                push_range(&mut out, start..(start + bs).min(len));
            }
        }
        Ok(out)
    }
}

impl Drop for BlockStore {
    fn drop(&mut self) {
        if let Err(e) = self.shutdown() {
            tracing::warn!(error = %e, "block store shutdown failed");
        }
    }
}

/// Appends `r` to `out`, merging with the last range when they touch.
pub(crate) fn push_range(out: &mut Vec<Range<u64>>, r: Range<u64>) {
    if let Some(last) = out.last_mut()
        && last.end == r.start
    {
        last.end = r.end;
        return;
    }
    out.push(r);
}

/// Runs in the first transaction after opening. Validates metadata, reconciles pack files with the
/// `packs` table and marks the store as not cleanly shut down.
/// Returns (pack to resume appending to, next pack id).
fn recover(t: &mut Tables<'_>, pack_dir: &Path, cfg: &StoreConfig) -> Result<(Option<u32>, u32)> {
    match t.meta(META_BLOCK_SIZE)? {
        None => {
            t.put_meta(META_BLOCK_SIZE, cfg.block_size as u64)?;
            t.put_meta(META_SCHEMA_VERSION, SCHEMA_VERSION)?;
        }
        Some(bs) if bs != cfg.block_size as u64 => {
            return Err(Error::Config(format!(
                "store was created with block_size {bs}"
            )));
        }
        Some(_) => {}
    }
    if t.meta(META_SCHEMA_VERSION)? != Some(SCHEMA_VERSION) {
        return Err(Error::Config("unsupported index schema version".into()));
    }
    let clean = t.meta(META_CLEAN_SHUTDOWN)? == Some(1);
    t.put_meta(META_CLEAN_SHUTDOWN, 0)?;

    let on_disk: HashSet<u32> = list_pack_ids(pack_dir)?.into_iter().collect();
    let mut active = Vec::new();
    let mut registered = HashSet::new();
    for (id, info) in t.packs()? {
        registered.insert(id);
        if info.state == PackState::Retired {
            match remove_pack_file(pack_dir, id) {
                Ok(()) => t.remove_pack(id)?,
                // Stays retired; deletion is retried on the next open.
                Err(e) => tracing::warn!(pack = id, error = %e, "could not delete retired pack"),
            }
            continue;
        }
        if !on_disk.contains(&id) {
            if info.live_bytes == 0 {
                t.remove_pack(id)?;
                continue;
            }
            return Err(Error::Corrupt(format!("pack {id} is missing")));
        }
        if info.state == PackState::Active {
            active.push(id);
        }
    }
    // Pack files created after the last durable commit hold no referenced data. One that cannot
    // be deleted now (another program holds it) is retried on the next open.
    for &id in &on_disk {
        if !registered.contains(&id)
            && let Err(e) = remove_pack_file(pack_dir, id)
        {
            tracing::warn!(pack = id, error = %e, "could not delete orphan pack file");
        }
    }
    // Resume the newest active pack only after a clean shutdown; otherwise its tail may be torn.
    active.sort_unstable();
    let resume = if clean { active.pop() } else { None };
    for id in active {
        let mut info = t.pack(id)?.unwrap();
        info.state = PackState::Sealed;
        t.put_pack(id, &info)?;
    }
    // Past every registered pack and every file on disk, so a new pack never collides with an
    // orphan file that could not be deleted.
    let max_seen = registered
        .iter()
        .chain(&on_disk)
        .copied()
        .max()
        .unwrap_or(0);
    let next = (t.meta(META_NEXT_PACK_ID)?.unwrap_or(1) as u32).max(max_seen + 1);
    Ok((resume, next))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const BS: usize = 4096;

    pub(crate) fn test_config() -> StoreConfig {
        StoreConfig {
            block_size: BS as u32,
            max_pack_size: 64 * 1024,
            index_cache_bytes: 4 << 20,
            write_txn_bytes: 8 * BS,
            ..StoreConfig::default()
        }
    }

    pub(crate) fn random_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        blake3::Hasher::new()
            .update(&seed.to_le_bytes())
            .finalize_xof()
            .fill(&mut out);
        out
    }

    pub(crate) fn read_all(store: &BlockStore, id: &[u8]) -> Vec<u8> {
        let len = store.stat(id).unwrap().unwrap().len as usize;
        let mut buf = vec![0u8; len];
        let r = store.read(id, 0, &mut buf).unwrap();
        assert!(r.missing.is_empty(), "missing {:?}", r.missing);
        buf
    }

    pub(crate) fn put(store: &BlockStore, id: &[u8], data: &[u8]) -> Result<()> {
        store.set_len(id, data.len() as u64)?;
        store.write_blocks(id, 0, data)
    }

    /// Writers that cross the auto-flush threshold together make one durable flush between
    /// them, not one each.
    #[test]
    fn writers_crossing_the_flush_threshold_together_flush_once() {
        let dir = tempfile::tempdir().unwrap();
        let threshold = 256 << 10;
        let store = BlockStore::open(
            dir.path(),
            StoreConfig {
                max_pack_size: 1 << 30,
                auto_flush_bytes: threshold,
                auto_flush_commits: u64::MAX,
                ..test_config()
            },
        )
        .unwrap();
        let before = store.index.stats().durable_commits;
        let (threads, files, size) = (16u64, 32u64, 4 * BS);
        let barrier = std::sync::Barrier::new(threads as usize);
        std::thread::scope(|s| {
            for t in 0..threads {
                let (store, barrier) = (&store, &barrier);
                s.spawn(move || {
                    barrier.wait();
                    for f in 0..files {
                        let id = format!("t{t}f{f}");
                        put(store, id.as_bytes(), &random_bytes(t * 1000 + f, size)).unwrap();
                    }
                });
            }
        });
        let durable = store.index.stats().durable_commits - before;
        // Each flush needs `threshold` new bytes after the one before it.
        let most = (threads * files) * (size as u64 + 64) / threshold + 1;
        assert!(
            durable >= 1 && durable <= most,
            "{durable} durable flushes, at most {most}"
        );
        assert!(store.verify().unwrap().is_ok());
    }

    #[test]
    fn failed_append_moves_later_writes_to_a_new_pack() {
        let dir = tempfile::tempdir().unwrap();
        let a = random_bytes(1, 2 * BS);
        let b = random_bytes(2, 2 * BS);
        let c = random_bytes(3, 2 * BS);
        {
            let store = BlockStore::open(dir.path(), test_config()).unwrap();
            put(&store, b"a", &a).unwrap();
            let first = store.writer.lock().unwrap().packs.active_id().unwrap();
            store.writer.lock().unwrap().packs.fail_next_append = true;
            assert!(put(&store, b"b", &b).is_err());
            put(&store, b"c", &c).unwrap();
            let now = store.writer.lock().unwrap().packs.active_id().unwrap();
            assert_ne!(
                now, first,
                "writes after a failed append must use a new pack"
            );
            let packs = store.index.read().unwrap().packs().unwrap();
            let info = packs.iter().find(|p| p.0 == first).unwrap().1;
            assert_eq!(info.state, PackState::Sealed);
            assert_eq!(read_all(&store, b"a"), a);
            assert_eq!(read_all(&store, b"c"), c);
            assert!(store.verify().unwrap().is_ok());
            store.close().unwrap();
        }
        let store = BlockStore::open(dir.path(), test_config()).unwrap();
        assert_eq!(read_all(&store, b"a"), a);
        assert_eq!(read_all(&store, b"c"), c);
        assert!(store.verify().unwrap().is_ok());
    }
}
