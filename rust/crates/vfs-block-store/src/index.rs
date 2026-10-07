//! Metadata index on redb. This is the only module that touches redb; nothing outside it sees redb types.

use std::cell::OnceCell;
use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use redb::{
    AccessGuard, Database, Durability, Key, ReadOnlyTable, ReadTransaction, ReadableDatabase,
    ReadableTable, Table, TableDefinition, Value,
};

use crate::codec::{Hash128, HEADER_LEN};
use crate::error::{Error, Result};

/// Manifest segments, keyed by (file id, segment number).
type FileKey = (&'static [u8], u32);

const FILES: TableDefinition<FileKey, &[u8]> = TableDefinition::new("files");
const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");
const DEDUP: TableDefinition<Hash128, u64> = TableDefinition::new("dedup");
const PACKS: TableDefinition<u32, &[u8]> = TableDefinition::new("packs");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

pub const META_SCHEMA_VERSION: &str = "schema_version";
pub const META_BLOCK_SIZE: &str = "block_size";
pub const META_NEXT_BLOCK_ID: &str = "next_block_id";
pub const META_NEXT_PACK_ID: &str = "next_pack_id";
pub const META_CLEAN_SHUTDOWN: &str = "clean_shutdown";
pub const SCHEMA_VERSION: u64 = 1;

/// Location and refcount of one stored block (row of the `blocks` table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockLoc {
    pub pack: u32,
    pub offset: u64,
    pub stored_len: u32,
    pub raw_len: u32,
    pub refcount: u32,
    pub hash: Hash128,
}

impl BlockLoc {
    pub const ENCODED_LEN: usize = 40;

    pub fn encode(&self) -> [u8; Self::ENCODED_LEN] {
        let mut b = [0u8; Self::ENCODED_LEN];
        b[0..4].copy_from_slice(&self.pack.to_le_bytes());
        b[4..12].copy_from_slice(&self.offset.to_le_bytes());
        b[12..16].copy_from_slice(&self.stored_len.to_le_bytes());
        b[16..20].copy_from_slice(&self.raw_len.to_le_bytes());
        b[20..24].copy_from_slice(&self.refcount.to_le_bytes());
        b[24..40].copy_from_slice(&self.hash);
        b
    }

    pub fn decode(b: &[u8]) -> Self {
        Self {
            pack: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            offset: u64::from_le_bytes(b[4..12].try_into().unwrap()),
            stored_len: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            raw_len: u32::from_le_bytes(b[16..20].try_into().unwrap()),
            refcount: u32::from_le_bytes(b[20..24].try_into().unwrap()),
            hash: b[24..40].try_into().unwrap(),
        }
    }

    /// Bytes the record occupies in its pack.
    pub fn record_len(&self) -> u64 {
        HEADER_LEN as u64 + self.stored_len as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackState {
    Active = 0,
    Sealed = 1,
    Retired = 2,
}

/// Row of the `packs` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackInfo {
    pub live_bytes: u64,
    pub state: PackState,
}

impl PackInfo {
    pub fn encode(&self) -> [u8; 9] {
        let mut b = [0u8; 9];
        b[0..8].copy_from_slice(&self.live_bytes.to_le_bytes());
        b[8] = self.state as u8;
        b
    }

    pub fn decode(b: &[u8]) -> Self {
        let state = match b[8] {
            0 => PackState::Active,
            1 => PackState::Sealed,
            _ => PackState::Retired,
        };
        Self {
            live_bytes: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            state,
        }
    }
}

/// Zero-copy view of a manifest segment value, valid while its `IndexRead` lives.
pub struct SegmentRef<'a>(AccessGuard<'a, &'static [u8]>);

impl Deref for SegmentRef<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.0.value()
    }
}

pub struct Index {
    db: Database,
    counters: IndexCounters,
}

/// What [`Index::update`] has cost since the index opened.
#[derive(Default)]
struct IndexCounters {
    commits: AtomicU64,
    durable: AtomicU64,
    wait_ns: AtomicU64,
    held_ns: AtomicU64,
}

/// Index transactions since the store opened (see [`crate::WriteStats::index`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// Write transactions committed, durable ones included.
    pub commits: u64,
    /// Of them, durable commits (each an fsync of the index).
    pub durable_commits: u64,
    /// Time writers waited for the index's one write transaction, ns summed over writers.
    pub wait_ns: u64,
    /// Time write transactions were open, commit included, ns.
    pub held_ns: u64,
}

impl Index {
    pub fn open(path: &Path, cache_bytes: usize) -> Result<Self> {
        let db = Database::builder()
            .set_cache_size(cache_bytes)
            .create(path)?;
        let index = Self {
            db,
            counters: IndexCounters::default(),
        };
        // Create all tables so read transactions can always open them.
        index.update(true, |_| Ok(()))?;
        Ok(index)
    }

    /// (bytes of pages in use, bytes of keys and values stored). Briefly takes the write lock.
    pub fn size(&self) -> Result<(u64, u64)> {
        let txn = self.db.begin_write()?;
        let s = txn.stats()?;
        let pages = s.allocated_pages() * s.page_size() as u64;
        let stored = s.stored_bytes();
        txn.abort()?;
        Ok((pages, stored))
    }

    /// Opens a snapshot for reading.
    pub fn read(&self) -> Result<IndexRead> {
        Ok(IndexRead {
            txn: self.db.begin_read()?,
            files: OnceCell::new(),
            blocks: OnceCell::new(),
            dedup: OnceCell::new(),
            packs: OnceCell::new(),
        })
    }

    /// Runs `f` in one write transaction and commits it if `f` returns `Ok`.
    /// `durable = false` commits with `Durability::None`. redb allows one write transaction at a
    /// time, so this blocks while another thread is inside `update`.
    pub fn update<R>(
        &self,
        durable: bool,
        f: impl FnOnce(&mut Tables<'_>) -> Result<R>,
    ) -> Result<R> {
        let t0 = Instant::now();
        let mut txn = self.db.begin_write()?;
        let t1 = Instant::now();
        let c = &self.counters;
        c.wait_ns
            .fetch_add((t1 - t0).as_nanos() as u64, Ordering::Relaxed);
        let held = scopeguard(|| {
            c.held_ns
                .fetch_add(t1.elapsed().as_nanos() as u64, Ordering::Relaxed);
        });
        txn.set_durability(if durable {
            Durability::Immediate
        } else {
            Durability::None
        })?;
        let result = {
            let mut tables = Tables {
                files: txn.open_table(FILES)?,
                blocks: txn.open_table(BLOCKS)?,
                dedup: txn.open_table(DEDUP)?,
                packs: txn.open_table(PACKS)?,
                meta: txn.open_table(META)?,
            };
            f(&mut tables)?
        };
        txn.commit()?;
        drop(held);
        c.commits.fetch_add(1, Ordering::Relaxed);
        if durable {
            c.durable.fetch_add(1, Ordering::Relaxed);
        }
        Ok(result)
    }

    pub fn stats(&self) -> IndexStats {
        let c = &self.counters;
        IndexStats {
            commits: c.commits.load(Ordering::Relaxed),
            durable_commits: c.durable.load(Ordering::Relaxed),
            wait_ns: c.wait_ns.load(Ordering::Relaxed),
            held_ns: c.held_ns.load(Ordering::Relaxed),
        }
    }
}

/// Runs `f` when dropped.
fn scopeguard(f: impl FnOnce()) -> impl Drop {
    struct Guard<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Guard<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f()
            }
        }
    }
    Guard(Some(f))
}

/// A read snapshot of the index. Tables are opened on first use, so a snapshot that only
/// touches one table (such as `stat`) pays for one.
pub struct IndexRead {
    txn: ReadTransaction,
    files: OnceCell<ReadOnlyTable<FileKey, &'static [u8]>>,
    blocks: OnceCell<ReadOnlyTable<u64, &'static [u8]>>,
    dedup: OnceCell<ReadOnlyTable<Hash128, u64>>,
    packs: OnceCell<ReadOnlyTable<u32, &'static [u8]>>,
}

fn lazy_table<'a, K: Key + 'static, V: Value + 'static>(
    txn: &ReadTransaction,
    cell: &'a OnceCell<ReadOnlyTable<K, V>>,
    def: TableDefinition<K, V>,
) -> Result<&'a ReadOnlyTable<K, V>> {
    if let Some(t) = cell.get() {
        return Ok(t);
    }
    let t = txn.open_table(def)?;
    Ok(cell.get_or_init(|| t))
}

impl IndexRead {
    pub fn segment(&self, file_id: &[u8], seg: u32) -> Result<Option<SegmentRef<'_>>> {
        Ok(lazy_table(&self.txn, &self.files, FILES)?
            .get((file_id, seg))?
            .map(SegmentRef))
    }

    pub fn block(&self, id: u64) -> Result<Option<BlockLoc>> {
        Ok(lazy_table(&self.txn, &self.blocks, BLOCKS)?
            .get(id)?
            .map(|g| BlockLoc::decode(g.value())))
    }

    pub fn dedup(&self, hash: &Hash128) -> Result<Option<u64>> {
        Ok(lazy_table(&self.txn, &self.dedup, DEDUP)?
            .get(hash)?
            .map(|g| g.value()))
    }

    pub fn pack(&self, id: u32) -> Result<Option<PackInfo>> {
        Ok(lazy_table(&self.txn, &self.packs, PACKS)?
            .get(id)?
            .map(|g| PackInfo::decode(g.value())))
    }

    pub fn packs(&self) -> Result<Vec<(u32, PackInfo)>> {
        let mut out = Vec::new();
        for e in lazy_table(&self.txn, &self.packs, PACKS)?.iter()? {
            let (k, v) = e?;
            out.push((k.value(), PackInfo::decode(v.value())));
        }
        Ok(out)
    }

    pub fn for_each_segment(
        &self,
        mut f: impl FnMut(&[u8], u32, &[u8]) -> Result<()>,
    ) -> Result<()> {
        for e in lazy_table(&self.txn, &self.files, FILES)?.iter()? {
            let (k, v) = e?;
            let (id, seg) = k.value();
            f(id, seg, v.value())?;
        }
        Ok(())
    }

    pub fn for_each_block(&self, mut f: impl FnMut(u64, BlockLoc) -> Result<()>) -> Result<()> {
        for e in lazy_table(&self.txn, &self.blocks, BLOCKS)?.iter()? {
            let (k, v) = e?;
            f(k.value(), BlockLoc::decode(v.value()))?;
        }
        Ok(())
    }

    pub fn for_each_dedup(&self, mut f: impl FnMut(Hash128, u64) -> Result<()>) -> Result<()> {
        for e in lazy_table(&self.txn, &self.dedup, DEDUP)?.iter()? {
            let (k, v) = e?;
            f(k.value(), v.value())?;
        }
        Ok(())
    }
}

/// The tables of one write transaction.
pub struct Tables<'t> {
    files: Table<'t, FileKey, &'static [u8]>,
    blocks: Table<'t, u64, &'static [u8]>,
    dedup: Table<'t, Hash128, u64>,
    packs: Table<'t, u32, &'static [u8]>,
    meta: Table<'t, &'static str, u64>,
}

impl Tables<'_> {
    pub fn segment(&self, file_id: &[u8], seg: u32) -> Result<Option<Vec<u8>>> {
        Ok(self.files.get((file_id, seg))?.map(|g| g.value().to_vec()))
    }

    pub fn put_segment(&mut self, file_id: &[u8], seg: u32, value: &[u8]) -> Result<()> {
        self.files.insert((file_id, seg), value)?;
        Ok(())
    }

    pub fn remove_segment(&mut self, file_id: &[u8], seg: u32) -> Result<()> {
        self.files.remove((file_id, seg))?;
        Ok(())
    }

    pub fn block(&self, id: u64) -> Result<Option<BlockLoc>> {
        Ok(self.blocks.get(id)?.map(|g| BlockLoc::decode(g.value())))
    }

    pub fn put_block(&mut self, id: u64, loc: &BlockLoc) -> Result<()> {
        self.blocks.insert(id, loc.encode().as_slice())?;
        Ok(())
    }

    pub fn dedup(&self, hash: &Hash128) -> Result<Option<u64>> {
        Ok(self.dedup.get(hash)?.map(|g| g.value()))
    }

    pub fn meta(&self, key: &str) -> Result<Option<u64>> {
        Ok(self.meta.get(key)?.map(|g| g.value()))
    }

    pub fn put_meta(&mut self, key: &str, value: u64) -> Result<()> {
        self.meta.insert(key, value)?;
        Ok(())
    }

    pub fn pack(&self, id: u32) -> Result<Option<PackInfo>> {
        Ok(self.packs.get(id)?.map(|g| PackInfo::decode(g.value())))
    }

    pub fn put_pack(&mut self, id: u32, info: &PackInfo) -> Result<()> {
        self.packs.insert(id, info.encode().as_slice())?;
        Ok(())
    }

    pub fn remove_pack(&mut self, id: u32) -> Result<()> {
        self.packs.remove(id)?;
        Ok(())
    }

    /// Adds `delta` (may be negative) to a pack's live byte count.
    pub fn add_live(&mut self, pack: u32, delta: i64) -> Result<()> {
        let mut info = self
            .pack(pack)?
            .ok_or_else(|| Error::Corrupt(format!("block references unknown pack {pack}")))?;
        info.live_bytes = info.live_bytes.checked_add_signed(delta).ok_or_else(|| {
            Error::Corrupt(format!("live byte accounting underflow in pack {pack}"))
        })?;
        self.put_pack(pack, &info)
    }

    /// Inserts a new block with refcount 0 and its dedup entry. Returns the new block id.
    pub fn insert_block(&mut self, loc: BlockLoc) -> Result<u64> {
        let id = self.meta(META_NEXT_BLOCK_ID)?.unwrap_or(1);
        self.put_meta(META_NEXT_BLOCK_ID, id + 1)?;
        let loc = BlockLoc { refcount: 0, ..loc };
        self.put_block(id, &loc)?;
        self.dedup.insert(&loc.hash, id)?;
        self.add_live(loc.pack, loc.record_len() as i64)?;
        Ok(id)
    }

    pub fn incref(&mut self, id: u64) -> Result<()> {
        let mut loc = self
            .block(id)?
            .ok_or_else(|| Error::Corrupt(format!("incref of unknown block {id}")))?;
        loc.refcount += 1;
        self.put_block(id, &loc)
    }

    /// Drops one reference. At zero the block's rows are removed and its pack's live bytes reduced.
    /// Decrementing a block that no longer exists (a healed block) is a no-op.
    pub fn decref(&mut self, id: u64) -> Result<()> {
        let Some(mut loc) = self.block(id)? else {
            return Ok(());
        };
        if loc.refcount > 1 {
            loc.refcount -= 1;
            return self.put_block(id, &loc);
        }
        self.remove_block(id, &loc)
    }

    /// Removes a block's `blocks` and `dedup` rows and its live bytes, regardless of refcount.
    pub fn remove_block(&mut self, id: u64, loc: &BlockLoc) -> Result<()> {
        self.blocks.remove(id)?;
        if self.dedup(&loc.hash)? == Some(id) {
            self.dedup.remove(&loc.hash)?;
        }
        self.add_live(loc.pack, -(loc.record_len() as i64))
    }

    pub fn packs(&self) -> Result<Vec<(u32, PackInfo)>> {
        let mut out = Vec::new();
        for e in self.packs.iter()? {
            let (k, v) = e?;
            out.push((k.value(), PackInfo::decode(v.value())));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> (tempfile::TempDir, Index) {
        let dir = vfs_testkit::tempdir().unwrap();
        let index = Index::open(&dir.path().join("index.redb"), 1 << 20).unwrap();
        (dir, index)
    }

    fn loc(pack: u32, hash: u8) -> BlockLoc {
        BlockLoc {
            pack,
            offset: 0,
            stored_len: 60,
            raw_len: 100,
            refcount: 0,
            hash: [hash; 16],
        }
    }

    #[test]
    fn block_loc_roundtrip() {
        let l = BlockLoc {
            pack: 3,
            offset: 1 << 33,
            stored_len: 5,
            raw_len: 6,
            refcount: 7,
            hash: [1; 16],
        };
        assert_eq!(BlockLoc::decode(&l.encode()), l);
        assert_eq!(l.record_len(), 45);
    }

    #[test]
    fn refcounting_frees_rows_and_live_bytes() {
        let (_d, index) = open();
        let id = index
            .update(false, |t| {
                t.put_pack(
                    1,
                    &PackInfo {
                        live_bytes: 0,
                        state: PackState::Active,
                    },
                )?;
                let id = t.insert_block(loc(1, 9))?;
                t.incref(id)?;
                t.incref(id)?;
                Ok(id)
            })
            .unwrap();
        assert_eq!(id, 1);
        let r = index.read().unwrap();
        assert_eq!(r.dedup(&[9; 16]).unwrap(), Some(1));
        assert_eq!(r.block(1).unwrap().unwrap().refcount, 2);
        assert_eq!(r.packs().unwrap()[0].1.live_bytes, 100);
        drop(r);

        index.update(false, |t| t.decref(id)).unwrap();
        assert_eq!(index.read().unwrap().block(1).unwrap().unwrap().refcount, 1);

        index.update(false, |t| t.decref(id)).unwrap();
        let r = index.read().unwrap();
        assert!(r.block(1).unwrap().is_none());
        assert!(r.dedup(&[9; 16]).unwrap().is_none());
        assert_eq!(r.packs().unwrap()[0].1.live_bytes, 0);
        drop(r);

        // decref of a missing block is a no-op
        index.update(false, |t| t.decref(id)).unwrap();
    }

    #[test]
    fn segments_are_zero_copy_readable() {
        let (_d, index) = open();
        index
            .update(false, |t| t.put_segment(b"file", 0, &[1, 2, 3]))
            .unwrap();
        let r = index.read().unwrap();
        assert_eq!(&*r.segment(b"file", 0).unwrap().unwrap(), &[1, 2, 3]);
        assert!(r.segment(b"file", 1).unwrap().is_none());
        assert!(r.segment(b"fil", 0).unwrap().is_none());
    }

    #[test]
    fn failed_update_is_not_committed() {
        let (_d, index) = open();
        let res: Result<()> = index.update(false, |t| {
            t.put_meta("x", 1)?;
            Err(Error::NotFound)
        });
        assert!(res.is_err());
        assert_eq!(index.update(false, |t| t.meta("x")).unwrap(), None);
    }
}
