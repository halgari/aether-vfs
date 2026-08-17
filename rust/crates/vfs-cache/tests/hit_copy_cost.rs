//! **Cost assertion, not a correctness one: a cache hit must not copy the whole
//! block.**
//!
//! Why this file exists. `vfs-cache`'s correctness suite passed in full while
//! `BlockCache::get` cloned the entire block on every hit, so a 4 KiB read
//! through a 1 MiB block memcpy'd 1 MiB and threw 99.6 % of it away. Nothing
//! measured cost, so nothing failed. The Node spike measured the result from the
//! outside: 24 MiB/s cached against ~1400 MiB/s raw.
//!
//! **What this measures: bytes heap-allocated on the hit path, per byte
//! delivered to the caller.** A counting `GlobalAlloc` wraps `System` and is
//! armed only around the measured reads. A hit that hands back a refcounted
//! handle allocates nothing; a hit that clones the block allocates one
//! block-sized buffer per read.
//!
//! **Why allocation and not wall-clock.** This is the one defect of the three
//! that has a fully deterministic instrument, and the allocator is *external* to
//! the code under test — it cannot be satisfied by a counter placed in the wrong
//! spot inside `BlockCache`, which is the failure mode this project has been
//! bitten by before. There is no timing in this file and therefore nothing here
//! is load- or machine-sensitive.
//!
//! **What would make it flaky, and why it is not.** The assertion is a *ratio*
//! (allocated bytes per delivered byte), so it is independent of the read count
//! and of the machine. The one real hazard is another thread allocating while
//! the counter is armed, which is why this lives in **its own test binary** with
//! exactly one `#[test]` — `#[global_allocator]` is process-global state, and
//! the project's convention (see `VA_LOCK`) is that such a test either takes a
//! lock or gets its own binary. The threshold is also nowhere near the line: the
//! expected value is ~0 allocated bytes and the defect allocated 256 bytes per
//! byte delivered, so any bound between the two separates them cleanly.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use vfs_cache::{BlockCache, CacheConfig, CachingProvider};
use vfs_provider::{Capabilities, DirEntry, Handle, Provider, Stat, VPath, KIND_FILE, OPEN_READ};

static ARMED: AtomicBool = AtomicBool::new(false);
static ALLOCATED: AtomicU64 = AtomicU64::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ARMED.load(Ordering::Relaxed) {
            ALLOCATED.fetch_add(new_size as u64, Ordering::Relaxed);
        }
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const BLOCK: u64 = 1024 * 1024;
const FILE_SIZE: u64 = 4 * 1024 * 1024;

/// Leaf whose reads are pure memcpy from an owned buffer. It also counts its own
/// calls, so the test can prove the reads it measured were *hits* rather than a
/// cache that quietly stopped caching — a fast, allocation-free number with the
/// source being re-read would mean the opposite of what this test claims.
struct MemLeaf {
    src: Vec<u8>,
    reads: AtomicU64,
}

impl Provider for MemLeaf {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            immutable: true,
            slow: true,
            ..Capabilities::read_only()
        }
    }
    fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
        Ok(Some(Stat {
            kind: KIND_FILE,
            size: FILE_SIZE,
            mtime: 7,
        }))
    }
    fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
        Ok(Vec::new())
    }
    fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
        Ok((1, FILE_SIZE, false))
    }
    fn close(&self, _h: Handle) -> Result<(), i32> {
        Ok(())
    }
    fn read_at(&self, _h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let start = (offset % self.src.len() as u64) as usize;
        let n = buf.len().min(self.src.len() - start);
        buf[..n].copy_from_slice(&self.src[start..start + n]);
        Ok(n)
    }
}

#[test]
fn a_cache_hit_does_not_allocate_a_block_sized_buffer() {
    const READ: usize = 4096;
    const HITS: usize = 2000;

    let leaf = Arc::new(MemLeaf {
        src: vec![0xABu8; BLOCK as usize],
        reads: AtomicU64::new(0),
    });
    let cache = Arc::new(BlockCache::new(CacheConfig {
        block_size: BLOCK,
        ram_budget: 8 * 1024 * 1024,
        disk_dir: None,
    }));
    let top = CachingProvider::new(leaf.clone(), cache.clone(), 1);
    let (h, size, _) = top.open(VPath::at_default("bench.bin"), OPEN_READ).unwrap();
    assert_eq!(size, FILE_SIZE);

    // Warm block 0 and let every lazily grown structure on the path (the open
    // table, the shard maps, the block itself) do its allocating before the
    // counter is armed. Everything measured below is a hit on this one block.
    let mut buf = vec![0u8; READ];
    assert_eq!(top.read_at(h, 0, &mut buf).unwrap(), READ);
    for i in 0..HITS.min(64) {
        top.read_at(h, (i * READ) as u64, &mut buf).unwrap();
    }
    let leaf_reads_before = leaf.reads.load(Ordering::Relaxed);
    let hits_before = cache.stats().hits;

    ALLOCATED.store(0, Ordering::Relaxed);
    ARMED.store(true, Ordering::Relaxed);
    for i in 0..HITS {
        let off = ((i % 64) * READ) as u64;
        assert_eq!(top.read_at(h, off, &mut buf).unwrap(), READ);
    }
    ARMED.store(false, Ordering::Relaxed);
    let allocated = ALLOCATED.load(Ordering::Relaxed);

    // Guard the measurement before trusting it: these reads must have been
    // hits, served from one 1 MiB block, with the source untouched.
    assert_eq!(
        leaf.reads.load(Ordering::Relaxed),
        leaf_reads_before,
        "the measured reads went to the source, so they were not cache hits"
    );
    assert_eq!(
        cache.stats().hits - hits_before,
        HITS as u64,
        "expected exactly one cache hit per read in the measured region"
    );

    let delivered = (HITS * READ) as u64;
    assert!(
        allocated < delivered,
        "a hit allocated more than it delivered: {allocated} bytes allocated to \
         deliver {delivered} bytes over {HITS} hits of {READ} B through a \
         {} KiB block ({:.1} allocated bytes per delivered byte). A hit must \
         hand back a reference to the cached block, not a copy of it.",
        BLOCK / 1024,
        allocated as f64 / delivered as f64
    );
    // The tighter statement of the same thing: per-hit allocation must not scale
    // with block size. One block-sized buffer per hit is the defect's signature.
    let per_hit = allocated / HITS as u64;
    assert!(
        per_hit < BLOCK / 64,
        "per-hit allocation {per_hit} B is within a factor of 64 of the {} KiB \
         block size — the hit path is still copying the block",
        BLOCK / 1024
    );

    top.close(h).unwrap();
}
