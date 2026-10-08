//! Tests of the pull-through cache: reads, fetch units, identity and budgets.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex};

use aether_block_store::StoreConfig;
use vfs_provider::{
    Access, Capabilities, CaseMatch, DirEntry, Handle, KIND_DIR, KIND_FILE, OPEN_READ, Provider,
    RootId, Stat, VPath, bad_fh, map_io_err, not_found,
};

use super::*;
use crate::config::StorageConfig;
use crate::storage::Storage;

pub(crate) const BS: usize = 4096;

pub(crate) fn small_cfg() -> StorageConfig {
    StorageConfig {
        store: StoreConfig {
            block_size: BS as u32,
            ..StoreConfig::default()
        },
        ..StorageConfig::default()
    }
}

pub(crate) fn temp_storage() -> (Arc<Storage>, tempfile::TempDir) {
    temp_storage_with(small_cfg())
}

pub(crate) fn temp_storage_with(cfg: StorageConfig) -> (Arc<Storage>, tempfile::TempDir) {
    let d = vfs_testkit::tempdir().unwrap();
    (Storage::open(d.path(), cfg).unwrap(), d)
}

/// A gate a source's `read_at` waits on until the test opens it.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    cv: Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut g = self.open.lock().unwrap();
        while !*g {
            g = self.cv.wait(g).unwrap();
        }
    }
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

/// Wraps any provider, declares it immutable and slow, and counts
/// `read_at` calls. `max_read` caps each read to force short reads.
pub(crate) struct Slow {
    inner: Arc<dyn Provider>,
    reads: AtomicU64,
    max_read: usize,
    gate: Option<Arc<Gate>>,
    /// Declared as `preferred_block`: `CachedSource`'s fetch unit.
    preferred_block: Option<u32>,
}

impl Slow {
    pub(crate) fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }
}

impl Provider for Slow {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            access: Access::Read,
            immutable: true,
            slow: true,
            preferred_block: self.preferred_block,
            case: self.inner.capabilities().case,
        }
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.inner.getattr(p)
    }
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.inner.readdir(p)
    }
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        self.inner.open(p, flags)
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.inner.close(h)
    }
    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if let Some(g) = &self.gate {
            g.wait();
        }
        let n = buf.len().min(self.max_read);
        self.inner.read_at(h, offset, &mut buf[..n])
    }
}

pub(crate) fn slow(inner: Arc<dyn Provider>) -> Arc<Slow> {
    Arc::new(Slow {
        inner,
        reads: AtomicU64::new(0),
        max_read: usize::MAX,
        gate: None,
        preferred_block: None,
    })
}

/// [`slow`], declaring a `preferred_block` of `unit` bytes.
fn hinted(inner: Arc<dyn Provider>, unit: usize) -> Arc<Slow> {
    Arc::new(Slow {
        inner,
        reads: AtomicU64::new(0),
        max_read: usize::MAX,
        gate: None,
        preferred_block: Some(unit as u32),
    })
}

/// Answers `stored_name` with a marker no listing could produce.
struct Marked(Arc<Slow>);

impl Provider for Marked {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.0.getattr(p)
    }
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.0.readdir(p)
    }
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        self.0.open(p, flags)
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.0.close(h)
    }
    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.0.read_at(h, offset, buf)
    }
    fn stored_name(&self, p: VPath) -> Result<Option<String>, i32> {
        Ok(Some(format!("marker:{}", p.rel)))
    }
}

#[test]
fn cached_provider_forwards_stored_name_to_its_source() {
    let (s, _d) = temp_storage();
    let p = s.cached(Arc::new(Marked(slow_fixture())), key());
    let got = p
        .stored_name(VPath::at_default("sub/B.txt"))
        .expect("forwarded, not unsupported");
    assert_eq!(got.as_deref(), Some("marker:sub/B.txt"));
}

pub(crate) fn slow_fixture() -> Arc<Slow> {
    slow(Arc::new(vfs_provider::conformance::MemFixture::new()))
}

/// A flat, root-blind, mutable map of files: `(body, mtime)`.
#[derive(Default)]
pub(crate) struct MapSource {
    files: Mutex<HashMap<String, (Vec<u8>, i64)>>,
    next: AtomicU64,
    opens: Mutex<HashMap<Handle, Vec<u8>>>,
}

impl MapSource {
    pub(crate) fn with(files: &[(&str, Vec<u8>)]) -> Arc<Self> {
        let s = MapSource::default();
        for (n, b) in files {
            s.set(n, b.clone(), 1);
        }
        Arc::new(s)
    }
    fn set(&self, name: &str, body: Vec<u8>, mtime: i64) {
        self.files
            .lock()
            .unwrap()
            .insert(name.to_string(), (body, mtime));
    }
}

impl Provider for MapSource {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            case: CaseMatch::Sensitive,
            ..Capabilities::read_only()
        }
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        if p.rel.is_empty() {
            return Ok(Some(Stat {
                kind: KIND_DIR,
                size: 0,
                mtime: 0,
            }));
        }
        Ok(self.files.lock().unwrap().get(p.rel).map(|(b, m)| Stat {
            kind: KIND_FILE,
            size: b.len() as u64,
            mtime: *m,
        }))
    }
    fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
        Ok(Vec::new())
    }
    fn open(&self, p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
        let body = self
            .files
            .lock()
            .unwrap()
            .get(p.rel)
            .ok_or_else(not_found)?
            .0
            .clone();
        let size = body.len() as u64;
        let h = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        self.opens.lock().map_err(|_| map_io_err())?.insert(h, body);
        Ok((h, size, false))
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.opens.lock().unwrap().remove(&h);
        Ok(())
    }
    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        let g = self.opens.lock().unwrap();
        let body = g.get(&h).ok_or_else(bad_fh)?;
        let start = (offset as usize).min(body.len());
        let n = (body.len() - start).min(buf.len());
        buf[..n].copy_from_slice(&body[start..start + n]);
        Ok(n)
    }
}

pub(crate) fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn read_all_at(p: &Arc<dyn Provider>, root: RootId, rel: &str) -> Vec<u8> {
    let (h, size, _) = p.open(VPath::new(root, rel), OPEN_READ).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0u8; 1000]; // not block-aligned on purpose
    loop {
        let n = p.read_at(h, out.len() as u64, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    p.close(h).unwrap();
    assert_eq!(out.len() as u64, size);
    out
}

pub(crate) fn read_all(p: &Arc<dyn Provider>, rel: &str) -> Vec<u8> {
    read_all_at(p, RootId::DEFAULT, rel)
}

pub(crate) fn key() -> SourceKey {
    SourceKey("k".into())
}

#[test]
fn conformance_through_the_cache() {
    let (s, _d) = temp_storage();
    vfs_provider::assert_conformance(s.cached(slow_fixture(), key()));
}

#[test]
fn second_read_is_served_from_the_store() {
    let (s, _d) = temp_storage();
    let src = slow_fixture();
    let p = s.cached(src.clone(), key());
    assert_eq!(read_all(&p, "a.txt"), b"hello");
    let before = src.reads();
    assert!(before > 0);
    assert_eq!(read_all(&p, "a.txt"), b"hello");
    assert_eq!(src.reads(), before, "no source read on the second pass");
}

#[test]
fn multi_block_file_with_short_reads_round_trips() {
    let (s, _d) = temp_storage();
    let body = pattern(3 * BS + 1234, 7);
    let map = MapSource::with(&[("big.bin", body.clone())]);
    let src = Arc::new(Slow {
        inner: map,
        reads: AtomicU64::new(0),
        max_read: 1000, // every block needs several source reads
        gate: None,
        preferred_block: None,
    });
    let p = s.cached(src.clone(), key());
    assert_eq!(read_all(&p, "big.bin"), body);
    let before = src.reads();

    // Reads that straddle block boundaries, served from RAM and store.
    let (h, _, _) = p.open(VPath::at_default("big.bin"), OPEN_READ).unwrap();
    let mut buf = vec![0u8; BS + 100];
    let n = p.read_at(h, BS as u64 - 50, &mut buf).unwrap();
    assert_eq!(&buf[..n], &body[BS - 50..BS - 50 + n]);
    assert!(n > 0);
    let n = p.read_at(h, (3 * BS + 1000) as u64, &mut buf).unwrap();
    assert_eq!(n, 234, "clamped at end of file");
    assert_eq!(&buf[..n], &body[3 * BS + 1000..]);
    assert_eq!(p.read_at(h, body.len() as u64 + 5, &mut buf).unwrap(), 0);
    p.close(h).unwrap();
    assert_eq!(src.reads(), before);

    let st = s.cache_stats();
    assert_eq!(st.misses, 4, "four blocks fetched once each");
    assert_eq!(st.bytes_from_source, body.len() as u64);
    assert!(st.hits > 0);
    assert!(st.bytes_from_cache > 0);
    assert_eq!(st.cached_logical_bytes, body.len() as u64);
    assert_eq!(st.store_write_errors, 0);
}

#[test]
fn only_stored_blocks_count_against_the_budget() {
    let d = vfs_testkit::tempdir().unwrap();
    let src = slow(MapSource::with(&[("big.bin", pattern(10 * BS, 5))]));
    {
        let s = Storage::open(d.path(), small_cfg()).unwrap();
        let p = s.cached(src.clone(), key());
        let (h, size, _) = p.open(VPath::at_default("big.bin"), OPEN_READ).unwrap();
        assert_eq!(size, 10 * BS as u64);
        assert_eq!(
            s.cache_stats().cached_logical_bytes,
            0,
            "opening stores nothing"
        );
        let mut probe = [0u8; 64];
        p.read_at(h, 0, &mut probe).unwrap();
        assert_eq!(
            s.cache_stats().cached_logical_bytes,
            BS as u64,
            "a header probe counts one block, not the whole file"
        );
        p.read_at(h, 0, &mut probe).unwrap();
        assert_eq!(s.cache_stats().cached_logical_bytes, BS as u64);
        p.close(h).unwrap();
        drop(p);
        s.close().unwrap();
    }
    let s = Storage::open(d.path(), small_cfg()).unwrap();
    assert_eq!(s.cache_stats().cached_logical_bytes, BS as u64);
    let rows = s.catalog.cache_all().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.logical_bytes, BS as u64);
    // A second block read after the reopen adds to the persisted count.
    let p = s.cached(src.clone(), key());
    let (h, _, _) = p.open(VPath::at_default("big.bin"), OPEN_READ).unwrap();
    p.read_at(h, BS as u64, &mut [0u8; 8]).unwrap();
    assert_eq!(s.cache_stats().cached_logical_bytes, 2 * BS as u64);
    p.close(h).unwrap();
}

#[test]
fn eviction_backs_off_while_open_files_hold_the_cache_over_budget() {
    let (s, _d) = temp_storage_with(StorageConfig {
        cache_max_bytes: 3 * BS as u64,
        ..small_cfg()
    });
    let src = slow(MapSource::with(&[("big.bin", pattern(40 * BS, 9))]));
    let p = s.cached(src.clone(), key());
    let (h, _, _) = p.open(VPath::at_default("big.bin"), OPEN_READ).unwrap();
    let mut buf = vec![0u8; BS];
    for b in 0..40u64 {
        assert_eq!(p.read_at(h, b * BS as u64, &mut buf).unwrap(), BS);
        // Each background run finishes before the next miss, so a missing
        // back-off would show up as one run per miss past the budget.
        s.wait_for_eviction();
    }
    let runs = s.cache.eviction_runs.load(Ordering::SeqCst);
    // One run gets stuck on the open file; a second only if the clock's
    // minute turned during the loop.
    assert!((1..=2).contains(&runs), "eviction ran {runs} times");
    assert_eq!(s.cache_stats().cached_logical_bytes, 40 * BS as u64);

    // Closing the file makes it evictable, which ends the back-off.
    p.close(h).unwrap();
    s.wait_for_eviction();
    assert!(s.cache_stats().cached_logical_bytes <= 3 * BS as u64 * 9 / 10);
}

/// A source whose `getattr` size disagrees with what `open` returns.
struct Liar(Arc<MapSource>);

impl Provider for Liar {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            immutable: true,
            slow: true,
            ..self.0.capabilities()
        }
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        Ok(self.0.getattr(p)?.map(|st| Stat {
            size: st.size + 1,
            ..st
        }))
    }
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.0.readdir(p)
    }
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        self.0.open(p, flags)
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.0.close(h)
    }
    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.0.read_at(h, offset, buf)
    }
}

#[test]
fn a_size_mismatch_between_getattr_and_open_is_served_uncached() {
    let (s, _d) = temp_storage();
    let src = slow(Arc::new(Liar(MapSource::with(&[(
        "a.txt",
        b"hello".to_vec(),
    )]))));
    let p = s.cached(src.clone(), key());
    assert_eq!(read_all(&p, "a.txt"), b"hello");
    let first = src.reads();
    assert_eq!(read_all(&p, "a.txt"), b"hello");
    assert!(src.reads() > first, "not cached, so read from the source");
    assert!(s.catalog.cache_all().unwrap().is_empty());
    assert_eq!(s.cache_stats().cached_logical_bytes, 0);
}

#[test]
fn store_serves_after_the_ram_tier_is_off() {
    let (s, _d) = temp_storage_with(StorageConfig {
        ram_tier_bytes: 0,
        ..small_cfg()
    });
    let src = slow_fixture();
    let p = s.cached(src.clone(), key());
    read_all(&p, "sub/b.txt");
    let before = src.reads();
    assert_eq!(read_all(&p, "sub/b.txt"), b"world!");
    assert_eq!(src.reads(), before);
    assert!(s.cache_stats().store_hits > 0);
}

#[test]
fn survives_reopen() {
    let d = vfs_testkit::tempdir().unwrap();
    let src = slow_fixture();
    {
        let s = Storage::open(d.path(), small_cfg()).unwrap();
        let p = s.cached(src.clone(), key());
        assert_eq!(read_all(&p, "a.txt"), b"hello");
        drop(p);
        s.close().unwrap();
    }
    let before = src.reads();
    let s = Storage::open(d.path(), small_cfg()).unwrap();
    assert_eq!(s.cache_stats().cached_logical_bytes, 5);
    let rows = s.catalog.cache_all().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.logical_bytes, 5);
    let p = s.cached(src.clone(), key());
    assert_eq!(read_all(&p, "a.txt"), b"hello");
    assert_eq!(src.reads(), before, "zero source reads after a reopen");
}

/// A file opened and closed without a read stores nothing: no catalog
/// row, no store file. Its first stored block creates both.
#[test]
fn opening_without_reading_stores_nothing() {
    let (s, _d) = temp_storage();
    let src = slow(MapSource::with(&[("a", pattern(3 * BS, 1))]));
    let p = s.cached(src, key());
    let (h, _, _) = p.open(VPath::at_default("a"), OPEN_READ).unwrap();
    p.close(h).unwrap();
    s.commit_access().unwrap();
    assert!(s.catalog.cache_all().unwrap().is_empty());
    assert!(s.store.file_ids().unwrap().is_empty());

    let (h, _, _) = p.open(VPath::at_default("a"), OPEN_READ).unwrap();
    let mut buf = [0u8; 10];
    p.read_at(h, BS as u64, &mut buf).unwrap();
    assert_eq!(buf[..], pattern(3 * BS, 1)[BS..BS + 10]);
    p.close(h).unwrap();
    s.commit_access().unwrap();
    let rows = s.catalog.cache_all().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].1.logical_bytes, BS as u64);
    assert_eq!(s.store.file_ids().unwrap().len(), 1);
    assert_eq!(s.cache_stats().cached_logical_bytes, BS as u64);
}

/// A damaged cache is bypassed, not fatal: a store read that fails, or a
/// cache file that cannot be opened, is served by the (healthy) source
/// and counted.
#[test]
fn a_damaged_cache_falls_back_to_the_source() {
    let (s, _d) = temp_storage_with(StorageConfig {
        ram_tier_bytes: 0, // every hit goes to the store
        ..small_cfg()
    });
    let body = pattern(3 * BS, 7);
    let src = slow(MapSource::with(&[("a", body.clone())]));
    let p = s.cached(src.clone(), key());
    assert_eq!(read_all(&p, "a"), body);

    s.cache.fail_store_reads.store(true, Ordering::SeqCst);
    let before = src.reads();
    assert_eq!(read_all(&p, "a"), body);
    assert!(src.reads() > before, "the source served the reads");
    assert!(
        s.cache_stats().store_read_errors >= 3,
        "{:?}",
        s.cache_stats()
    );
    s.cache.fail_store_reads.store(false, Ordering::SeqCst);

    s.cache.fail_acquire.store(true, Ordering::SeqCst);
    let before = src.reads();
    assert_eq!(read_all(&p, "a"), body);
    assert!(src.reads() > before, "served uncached");
    assert_eq!(s.cache_stats().bypassed_opens, 1);
}

#[test]
fn fast_or_mutable_sources_are_not_wrapped() {
    let (s, _d) = temp_storage();
    let fast: Arc<dyn Provider> = Arc::new(vfs_provider::conformance::MemFixture::new());
    assert!(Arc::ptr_eq(&s.cached(fast.clone(), key()), &fast));

    struct Caps(Capabilities);
    impl Provider for Caps {
        fn capabilities(&self) -> Capabilities {
            self.0
        }
        fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
            Ok(None)
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(Vec::new())
        }
        fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
            Err(not_found())
        }
        fn close(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }
    }
    let base = Capabilities::read_only();
    for c in [
        Capabilities {
            immutable: true,
            ..base
        },
        Capabilities { slow: true, ..base },
        Capabilities {
            access: Access::SeqRead,
            immutable: true,
            slow: true,
            ..base
        },
    ] {
        let p: Arc<dyn Provider> = Arc::new(Caps(c));
        assert!(Arc::ptr_eq(&s.cached(p.clone(), key()), &p), "{c:?}");
    }
    let both: Arc<dyn Provider> = Arc::new(Caps(Capabilities {
        immutable: true,
        slow: true,
        ..base
    }));
    let wrapped = s.cached(both.clone(), key());
    assert!(!Arc::ptr_eq(&wrapped, &both));
    assert!(!wrapped.capabilities().slow, "the cache answers `slow`");
}

#[test]
fn version_change_is_a_new_cache_file() {
    let (s, _d) = temp_storage();
    let map = MapSource::with(&[("a.txt", b"hello".to_vec())]);
    let p = s.cached(slow(map.clone()), key());
    assert_eq!(read_all(&p, "a.txt"), b"hello");

    map.set("a.txt", b"hello!!".to_vec(), 1); // new size
    assert_eq!(read_all(&p, "a.txt"), b"hello!!");

    map.set("a.txt", b"HELLO!!".to_vec(), 2); // same size, new mtime
    assert_eq!(read_all(&p, "a.txt"), b"HELLO!!");
    assert_eq!(s.catalog.cache_all().unwrap().len(), 3);
}

#[test]
fn the_root_and_the_source_key_are_part_of_the_identity() {
    let (s, _d) = temp_storage();
    let src = slow(MapSource::with(&[("a.txt", b"hello".to_vec())]));
    let p = s.cached(src.clone(), key());
    read_all_at(&p, RootId(0), "a.txt");
    let one = src.reads();
    read_all_at(&p, RootId(1), "a.txt");
    assert!(src.reads() > one, "another root is another file");
    let two = src.reads();
    let q = s.cached(src.clone(), SourceKey("other".into()));
    read_all_at(&q, RootId(0), "a.txt");
    assert!(src.reads() > two, "another source key is another file");
    assert_eq!(s.catalog.cache_all().unwrap().len(), 3);
}

#[test]
fn concurrent_misses_fetch_once() {
    const READERS: usize = 8;
    let (s, _d) = temp_storage();
    let body = pattern(BS, 3);
    let gate = Arc::new(Gate::default());
    let src = Arc::new(Slow {
        inner: MapSource::with(&[("f", body.clone())]),
        reads: AtomicU64::new(0),
        max_read: usize::MAX,
        gate: Some(gate.clone()),
        preferred_block: None,
    });
    let p = s.cached(src.clone(), key());
    let barrier = Arc::new(Barrier::new(READERS));
    let threads: Vec<_> = (0..READERS)
        .map(|_| {
            let (p, barrier) = (p.clone(), barrier.clone());
            std::thread::spawn(move || {
                let (h, _, _) = p.open(VPath::at_default("f"), OPEN_READ).unwrap();
                barrier.wait();
                let mut buf = vec![0u8; BS];
                let n = p.read_at(h, 0, &mut buf).unwrap();
                p.close(h).unwrap();
                buf.truncate(n);
                buf
            })
        })
        .collect();
    // Released only once one reader is inside the source and the other
    // seven have joined its fetch: no timing, only these two conditions.
    while src.reads() < 1 || s.cache.coalesced_waits.load(Ordering::SeqCst) < 7 {
        std::thread::yield_now();
    }
    gate.release();
    for t in threads {
        assert_eq!(t.join().unwrap(), body);
    }
    assert_eq!(
        src.reads(),
        1,
        "one source fetch for eight concurrent misses"
    );
    assert_eq!(s.cache_stats().misses, 1);
}

#[test]
fn unit_blocks_rounds_the_hint_up_to_whole_blocks_and_clamps_it() {
    assert_eq!(unit_blocks(None, 4096), 1);
    assert_eq!(
        unit_blocks(Some(1000), 4096),
        1,
        "a hint below a block is one block"
    );
    assert_eq!(unit_blocks(Some(4096), 4096), 1);
    assert_eq!(unit_blocks(Some(4 * 4096), 4096), 4);
    assert_eq!(
        unit_blocks(Some(4 * 4096 + 1), 4096),
        5,
        "rounded up, never down"
    );
    assert_eq!(
        unit_blocks(Some(4 << 20), 64 << 10),
        64,
        "a 4 MiB frame over 64 KiB blocks"
    );
    assert_eq!(
        unit_blocks(Some(u32::MAX), 64 << 10),
        1024,
        "clamped to 64 MiB"
    );
}

#[test]
fn conformance_through_a_hinted_cache() {
    let (s, _d) = temp_storage();
    let src = hinted(
        Arc::new(vfs_provider::conformance::MemFixture::new()),
        4 * BS,
    );
    vfs_provider::assert_conformance(s.cached(src, key()));
}

/// One miss fills the whole unit the source prefers, in one source read,
/// and every block of it is then served without the source — including
/// the short tail unit at the end of the file.
#[test]
fn a_miss_fetches_the_whole_preferred_unit_in_one_source_read() {
    let (s, _d) = temp_storage();
    let body = pattern(10 * BS + 100, 4);
    let src = hinted(MapSource::with(&[("f", body.clone())]), 4 * BS);
    let p = s.cached(src.clone(), key());
    let (h, _, _) = p.open(VPath::at_default("f"), OPEN_READ).unwrap();
    let mut buf = [0u8; 10];

    p.read_at(h, BS as u64 + 5, &mut buf).unwrap();
    assert_eq!(buf[..], body[BS + 5..BS + 15]);
    assert_eq!(src.reads(), 1, "one source read for the unit");
    let st = s.cache_stats();
    assert_eq!(st.misses, 4, "four blocks fetched");
    assert_eq!(st.bytes_from_source, 4 * BS as u64);
    assert_eq!(st.cached_logical_bytes, 4 * BS as u64, "all four stored");

    for b in [0u64, 2, 3] {
        p.read_at(h, b * BS as u64, &mut buf).unwrap();
        assert_eq!(buf[..], body[b as usize * BS..b as usize * BS + 10]);
    }
    assert_eq!(src.reads(), 1, "the rest of the unit came from the cache");

    p.read_at(h, 4 * BS as u64, &mut buf).unwrap();
    assert_eq!(src.reads(), 2, "the next unit is its own fetch");
    let mut tail = [0u8; 100];
    assert_eq!(p.read_at(h, 10 * BS as u64, &mut tail).unwrap(), 100);
    assert_eq!(tail[..], body[10 * BS..]);
    assert_eq!(
        src.reads(),
        3,
        "the tail unit: blocks 8, 9 and the short 10"
    );
    assert_eq!(
        s.cache_stats().cached_logical_bytes,
        8 * BS as u64 + 2 * BS as u64 + 100
    );
    p.close(h).unwrap();
    assert_eq!(read_all(&p, "f"), body);
    assert_eq!(src.reads(), 3, "the whole file is cached");
}

/// A unit partly stored by an earlier reader (here one with no hint) is
/// completed with one source read spanning its missing blocks, and a
/// block stored twice is counted once against the budget.
#[test]
fn a_partly_stored_unit_fetches_only_its_missing_span_and_counts_each_block_once() {
    let (s, _d) = temp_storage();
    let body = pattern(4 * BS, 6);
    let map = MapSource::with(&[("f", body.clone())]);
    let plain = s.cached(slow(map.clone()), key());
    let (h, _, _) = plain.open(VPath::at_default("f"), OPEN_READ).unwrap();
    plain.read_at(h, 0, &mut [0u8; 8]).unwrap();
    plain.read_at(h, BS as u64, &mut [0u8; 8]).unwrap();
    plain.close(h).unwrap();
    assert_eq!(s.cache_stats().cached_logical_bytes, 2 * BS as u64);
    let before = s.cache_stats().bytes_from_source;

    // Same key and path, so the same cache file; a 4-block unit.
    let src = hinted(map, 4 * BS);
    let p = s.cached(src.clone(), key());
    let (h, _, _) = p.open(VPath::at_default("f"), OPEN_READ).unwrap();
    let mut buf = [0u8; 10];
    p.read_at(h, 3 * BS as u64, &mut buf).unwrap();
    assert_eq!(buf[..], body[3 * BS..3 * BS + 10]);
    assert_eq!(src.reads(), 1);
    assert_eq!(
        s.cache_stats().bytes_from_source - before,
        2 * BS as u64,
        "only blocks 2 and 3 were read from the source"
    );
    assert_eq!(s.cache_stats().cached_logical_bytes, 4 * BS as u64);
    p.close(h).unwrap();
    assert_eq!(read_all(&p, "f"), body);
    assert_eq!(src.reads(), 1);
}

/// Readers of *different* blocks of one unit share one fetch.
#[test]
fn concurrent_misses_on_one_unit_fetch_once() {
    const READERS: usize = 8;
    let (s, _d) = temp_storage();
    let body = pattern(READERS * BS, 8);
    let gate = Arc::new(Gate::default());
    let src = Arc::new(Slow {
        inner: MapSource::with(&[("f", body.clone())]),
        reads: AtomicU64::new(0),
        max_read: usize::MAX,
        gate: Some(gate.clone()),
        preferred_block: Some((READERS * BS) as u32),
    });
    let p = s.cached(src.clone(), key());
    let barrier = Arc::new(Barrier::new(READERS));
    let threads: Vec<_> = (0..READERS)
        .map(|i| {
            let (p, barrier) = (p.clone(), barrier.clone());
            std::thread::spawn(move || {
                let (h, _, _) = p.open(VPath::at_default("f"), OPEN_READ).unwrap();
                barrier.wait();
                let mut buf = vec![0u8; BS];
                let n = p.read_at(h, (i * BS) as u64, &mut buf).unwrap();
                p.close(h).unwrap();
                buf.truncate(n);
                (i, buf)
            })
        })
        .collect();
    while src.reads() < 1 || s.cache.coalesced_waits.load(Ordering::SeqCst) < 7 {
        std::thread::yield_now();
    }
    gate.release();
    for t in threads {
        let (i, got) = t.join().unwrap();
        assert_eq!(got, body[i * BS..(i + 1) * BS]);
    }
    assert_eq!(
        src.reads(),
        1,
        "one source fetch for eight blocks of one unit"
    );
    assert_eq!(s.cache_stats().misses, READERS as u64);
}

/// Two `CachedSource`s over one `Storage` and cache file (same
/// `SourceKey`, path, size and mtime — see `identity`) but different
/// `preferred_block` units must never cross wires, even though their
/// naive unit index `b / unit` can coincide: X's 4-block unit puts block
/// 5 in "unit 1" (blocks 4..8); Y has no hint (a 1-block unit), and its
/// miss on block 1 is also "unit 1" by index alone. Racing them exercises
/// the in-flight map keyed by actual fetch geometry, not just that index.
#[test]
fn concurrent_misses_with_different_units_never_cross_wires() {
    let (s, _d) = temp_storage();
    let body = pattern(8 * BS, 9);
    let map = MapSource::with(&[("f", body.clone())]);
    let gate = Arc::new(Gate::default());
    let src_x = Arc::new(Slow {
        inner: map.clone(),
        reads: AtomicU64::new(0),
        max_read: usize::MAX,
        gate: Some(gate.clone()),
        preferred_block: Some((4 * BS) as u32),
    });
    let src_y = Arc::new(Slow {
        inner: map,
        reads: AtomicU64::new(0),
        max_read: usize::MAX,
        gate: Some(gate.clone()),
        preferred_block: None,
    });
    let x = s.cached(src_x.clone(), key());
    let y = s.cached(src_y.clone(), key());
    let (hx, _, _) = x.open(VPath::at_default("f"), OPEN_READ).unwrap();
    let (hy, _, _) = y.open(VPath::at_default("f"), OPEN_READ).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let tx = {
        let (x, barrier) = (x.clone(), barrier.clone());
        std::thread::spawn(move || {
            barrier.wait();
            let mut buf = vec![0u8; BS];
            x.read_at(hx, 5 * BS as u64, &mut buf).unwrap();
            x.close(hx).unwrap();
            buf
        })
    };
    let ty = {
        let (y, barrier) = (y.clone(), barrier.clone());
        std::thread::spawn(move || {
            barrier.wait();
            let mut buf = vec![0u8; BS];
            y.read_at(hy, BS as u64, &mut buf).unwrap();
            y.close(hy).unwrap();
            buf
        })
    };
    // Released only once both misses are inside their source, so the
    // in-flight map holds both cells at once.
    while src_x.reads() < 1 || src_y.reads() < 1 {
        std::thread::yield_now();
    }
    gate.release();
    let got_x = tx.join().unwrap();
    let got_y = ty.join().unwrap();
    assert_eq!(got_x[..], body[5 * BS..6 * BS], "X got its own block 5");
    assert_eq!(
        got_y[..],
        body[BS..2 * BS],
        "Y got its own block 1, not X's block 4"
    );
    assert_eq!(src_x.reads(), 1);
    assert_eq!(src_y.reads(), 1);
}

#[test]
fn cached_coverage_reports_what_the_cache_holds_of_a_file() {
    let (s, _d) = temp_storage();
    let body = pattern(3 * BS + 10, 2);
    let src = slow(MapSource::with(&[("f", body.clone())]));
    let p = s.cached(src.clone(), key());
    let at = VPath::at_default("f");
    assert!(
        s.cached_coverage(&*src, &key(), at).unwrap().is_empty(),
        "nothing yet"
    );

    let (h, _, _) = p.open(at, OPEN_READ).unwrap();
    p.read_at(h, BS as u64 + 1, &mut [0u8; 4]).unwrap();
    p.close(h).unwrap();
    assert_eq!(
        s.cached_coverage(&*src, &key(), at).unwrap(),
        vec![BS as u64..2 * BS as u64]
    );

    read_all(&p, "f");
    assert_eq!(
        s.cached_coverage(&*src, &key(), at).unwrap(),
        vec![0..body.len() as u64],
        "merged into one range, the short tail block included"
    );
    let reads = src.reads();
    assert!(
        s.cached_coverage(&*src, &SourceKey("other".into()), at)
            .unwrap()
            .is_empty()
    );
    assert!(
        s.cached_coverage(&*src, &key(), VPath::at_default("missing"))
            .unwrap()
            .is_empty()
    );
    assert!(
        s.cached_coverage(&*src, &key(), VPath::at_default(""))
            .unwrap()
            .is_empty(),
        "a directory"
    );
    assert_eq!(src.reads(), reads, "coverage never reads the source");
}

pub(crate) fn write_layer_file(s: &Arc<Storage>, layer: &str, rel: &str, body: &[u8]) {
    use vfs_provider::{OPEN_CREATE, OPEN_WRITE};
    let l = s.layer(layer).unwrap();
    let (h, _, _) = l
        .open(VPath::at_default(rel), OPEN_CREATE | OPEN_WRITE | OPEN_READ)
        .unwrap();
    let mut off = 0;
    while off < body.len() {
        off += l.write_at(h, off as u64, &body[off..]).unwrap();
    }
    l.close(h).unwrap();
}

#[test]
fn the_callers_write_class_reaches_the_store() {
    use aether_block_store::{WriteClass, with_write_class};
    let (s, _d) = temp_storage();
    let (a, b) = (pattern(3 * BS, 6), pattern(2 * BS + 5, 7));
    let p = s.cached(
        slow(MapSource::with(&[("a", a.clone()), ("b", b.clone())])),
        key(),
    );
    with_write_class(WriteClass::Bulk, || assert_eq!(read_all(&p, "a"), a));
    assert_eq!(read_all(&p, "b"), b);
    let body = pattern(5 * BS, 8);
    with_write_class(WriteClass::Bulk, || {
        write_layer_file(&s, "content", "x", &body)
    });
    let st = s.write_stats();
    assert_eq!(st.bulk.logical_bytes, (a.len() + body.len()) as u64);
    assert_eq!(st.foreground.logical_bytes, b.len() as u64);
    assert_eq!(
        s.written().logical_bytes,
        (a.len() + b.len() + body.len()) as u64
    );
    assert_eq!(s.compression(WriteClass::Bulk), "zstd:6");
}
