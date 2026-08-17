//! **Cost assertions for the two hit-path defects that only a clock can see:**
//! per-hit work that grows with the number of resident blocks (the O(n) LRU
//! scan), and hits that do not run concurrently (one process-wide mutex).
//!
//! Neither has a deterministic instrument the way the block clone does — an
//! O(n) scan allocates nothing and a lock consumes no bytes — so these are
//! wall-clock tests. Everything here is therefore built to be *hard to make
//! flaky*, and the reasoning is written down rather than implied, because a
//! performance test that someone later mutes is worse than no test at all.
//!
//! **Every assertion is a ratio between two measurements taken seconds apart on
//! the same machine, in the same process, over the same data.** No absolute
//! µs or MiB/s figure appears in an assertion. A slow or loaded machine moves
//! both sides of every ratio together.
//!
//! **The thresholds sit an order of magnitude away from both outcomes.** As
//! measured on the machine that found these defects (`spike-node/cache-cost`, a
//! throwaway harness since deleted — see docs/benchmarks/block-cache-hit-cost.md):
//! the residency ratio was **34x** before the fix and ~**1x** after, and the
//! thread ratio was **0.7x** before (throughput *fell* with more threads) and
//! ~**3x** after. The gates below are 4x and 1.8x. Nothing needs to be re-tuned
//! for a faster or slower machine; a regression has to travel most of an order
//! of magnitude to trip one.
//!
//! **Known limits, stated so nobody has to guess:**
//! * The thread test needs cores. It **skips** below 4 available and says so.
//!   A skip is a pass — on a 2-core CI box this test proves nothing.
//! * Both tests take the best of several attempts. That is deliberate and it is
//!   sound for the claim being made: "the cache *can* serve hits concurrently"
//!   and "per-hit work *is* independent of residency" are statements about the
//!   implementation, not guarantees about a contended machine. Best-of-N cannot
//!   turn a serialising cache into a scaling one — the defect fails all N.
//! * They are excluded from wall-clock claims in debug builds only by being
//!   ratios; both sides pay the same unoptimised price.
//! * They serialise against each other on `PERF_LOCK`, per the project's
//!   test-isolation convention, because each wants the machine to itself.

use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vfs_cache::{BlockCache, BlockKey, CacheConfig, CachingProvider};
use vfs_provider::{Capabilities, DirEntry, Handle, Provider, Stat, VPath, KIND_FILE, OPEN_READ};

/// Wall-clock tests in one binary must not run concurrently: cargo runs `#[test]`
/// fns in parallel threads by default, and two throughput measurements racing
/// each other for cores is exactly the flakiness this file is trying to avoid.
static PERF_LOCK: Mutex<()> = Mutex::new(());

/// Take `PERF_LOCK`, ignoring poisoning. A failing perf test panics while
/// holding it, and `unwrap()` here would turn one genuine failure into three
/// `PoisonError`s that hide which assertion actually fired — the exact way a
/// suite starts lying about what it caught.
fn perf_lock() -> std::sync::MutexGuard<'static, ()> {
    PERF_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn key(i: u64) -> BlockKey {
    BlockKey {
        source_id: 1,
        file_id: 42,
        block_index: i,
    }
}

/// Fill `blocks` blocks of `block_size` and return the cache. `ram_budget` is
/// identical across the configurations a test compares, so shard geometry and
/// eviction pressure are held constant and residency is the only variable.
fn filled(block_size: u64, blocks: u64, ram_budget: u64) -> Arc<BlockCache> {
    let cache = Arc::new(BlockCache::new(CacheConfig {
        block_size,
        ram_budget,
        disk_dir: None,
    }));
    for i in 0..blocks {
        cache.put(key(i), vec![(i % 251) as u8; block_size as usize]);
    }
    let resident = cache.stats().ram_blocks;
    assert_eq!(
        resident, blocks,
        "the fixture evicted while filling ({resident} of {blocks} resident); \
         the comparison below would be measuring eviction, not hit cost"
    );
    cache
}

/// Nanoseconds per hit on one hot block, with `resident` other blocks in the
/// cache. Reads `out_len` bytes out of the returned block so the per-op work
/// outside the lookup is identical in every configuration.
fn hot_block_ns(cache: &Arc<BlockCache>, ops: usize, out_len: usize) -> f64 {
    let k = key(0);
    let mut sink = vec![0u8; out_len];
    // One untimed hit: on an LRU deque the hot block starts at the *front*
    // (least recently used), where a front-to-back scan finds it immediately.
    // It is only from the second hit onward — with the block sitting at the
    // back — that the scan costs what it costs.
    let warm = cache.get(&k).expect("hot block resident");
    sink.copy_from_slice(&warm[..out_len]);
    let t0 = Instant::now();
    for _ in 0..ops {
        let b = cache.get(&k).expect("hot block resident");
        sink.copy_from_slice(&b[..out_len]);
        black_box(&sink);
    }
    let ns = t0.elapsed().as_nanos() as f64 / ops as f64;
    black_box(sink);
    ns
}

/// **Defect 2.** Per-hit cost must not grow with the number of resident blocks.
///
/// Re-reading one block while the rest of a file stays resident is an ordinary
/// pattern (a header, an index, a manifest) and it is the worst case for an LRU
/// deque scanned front-to-back: after the first hit the block sits at the back,
/// so every later hit walks the whole deque. The bytes touched per read are
/// identical in both configurations — one 4 KiB block, the same one, in and out
/// of L1 — so a ratio above 1 is the ordering structure and can be little else.
#[test]
fn per_hit_cost_does_not_grow_with_resident_block_count() {
    let _g = perf_lock();
    const BS: u64 = 4096;
    const BUDGET: u64 = 192 * 1024 * 1024;
    const OPS: usize = 40_000;

    let small = filled(BS, 64, BUDGET);
    let large = filled(BS, 16_384, BUDGET);

    // Best of three, alternating, so a transient stall on the machine cannot
    // land on only one side of the ratio.
    let mut best = f64::MAX;
    let (mut s_best, mut l_best) = (f64::MAX, f64::MAX);
    for _ in 0..3 {
        let s = hot_block_ns(&small, OPS, BS as usize);
        let l = hot_block_ns(&large, OPS, BS as usize);
        s_best = s_best.min(s);
        l_best = l_best.min(l);
        best = best.min(l / s);
    }
    let ratio = (l_best / s_best).min(best);

    assert!(
        ratio < 4.0,
        "hit cost grew {ratio:.1}x when residency grew 256x (64 blocks: \
         {s_best:.0} ns/hit, 16384 blocks: {l_best:.0} ns/hit). A hit must not \
         scan the eviction ordering. Measured 34x with the O(n) LRU scan in \
         place and ~1x without it; the gate is 4x."
    );
}

/// Aggregate ops/sec for `work` run on `threads` threads, all released from a
/// barrier together so the timed region is the concurrent part.
fn aggregate_ops_per_sec<F>(threads: usize, ops_per_thread: usize, out_len: usize, work: F) -> f64
where
    F: Fn(usize, u64, &mut [u8]) + Send + Sync + 'static,
{
    let work = Arc::new(work);
    let start = Arc::new(std::sync::Barrier::new(threads + 1));
    let mut handles = Vec::with_capacity(threads);
    for t in 0..threads {
        let work = Arc::clone(&work);
        let start = Arc::clone(&start);
        handles.push(std::thread::spawn(move || {
            let mut sink = vec![0u8; out_len];
            start.wait();
            for i in 0..ops_per_thread as u64 {
                work(t, i, &mut sink);
            }
            black_box(sink);
        }));
    }
    start.wait();
    let t0 = Instant::now();
    for h in handles {
        h.join().expect("reader thread");
    }
    (threads * ops_per_thread) as f64 / t0.elapsed().as_secs_f64()
}

/// **Defect 3.** Concurrent hits must not be serialised on the cache's lock.
///
/// The whole cache sat behind a single `Mutex`, so the spike measured cached
/// throughput flat at 24 -> 26 MiB/s from one to eight threads while p50 grew
/// linearly, 155 -> 1139 µs. In this harness the same defect showed as
/// throughput *falling*: 2012 -> 1277 MiB/s from 1 to 8 threads.
///
/// **This is a ratio of ratios, and the first version of this test was wrong.**
/// Asserting a bare 1-to-4-thread speedup for the cache looked reasonable and
/// was flaky: the per-op work is a memcpy out of the block, memcpy does not
/// scale linearly across cores sharing a cache hierarchy, and the fixed cache
/// landed near the gate on merit — 1.7x to 2.4x run to run. Raising the gate
/// would have hidden the defect; lowering it would have kept the flake. The fix
/// is to divide out the machine: the identical loop, over the identical blocks,
/// with the cache lookup replaced by a plain slice index, is measured in the
/// same process seconds apart. That baseline has **no synchronisation at all**,
/// so its 1-to-4 speedup *is* this machine's memory-bandwidth ceiling for this
/// workload. What is asserted is how much of that ceiling the cache keeps.
#[test]
fn concurrent_hits_scale_as_well_as_the_same_work_without_a_cache() {
    let _g = perf_lock();
    const BS: u64 = 64 * 1024;
    const BLOCKS: u64 = 64;
    const OPS: usize = 120_000;
    const OUT: usize = 4096;
    const THREADS: usize = 4;
    const ATTEMPTS: usize = 4;
    /// **Where this number comes from, and why it is not 0.95.**
    ///
    /// Measured on the machine that found the defect, 4 threads vs 1, best of
    /// four, repeated runs: **0.19-0.22 with the single process-wide mutex** and
    /// **0.58-0.95 after the fix**. The fixed cache does not reach the control's
    /// scaling and is not expected to: a hit still acquires and releases a
    /// shared lock word and bumps a refcount, and those are atomic operations on
    /// lines other cores also touch, while the control does nothing but memcpy.
    /// So the honest gate sits between the two populations rather than near the
    /// ideal — 2x above the defect, and below the worst fixed run observed.
    /// Raising it toward 0.9 would flake; lowering it under ~0.3 would stop
    /// separating a serialising cache from a scaling one.
    const GATE: f64 = 0.40;

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if cores < THREADS {
        eprintln!(
            "SKIP concurrent_hits_scale_as_well_as_the_same_work_without_a_cache: \
             {cores} cores available, this test needs {THREADS} to say anything. \
             A skip here is not a pass for the cache — it means the machine \
             cannot answer the question."
        );
        return;
    }

    let cache = filled(BS, BLOCKS, 32 * 1024 * 1024);
    // The control: the same payloads, reachable without touching the cache.
    let plain: Arc<Vec<Arc<[u8]>>> = Arc::new(
        (0..BLOCKS)
            .map(|i| -> Arc<[u8]> { vec![(i % 251) as u8; BS as usize].into() })
            .collect(),
    );

    // Each thread gets a **disjoint** slice of the blocks. That is deliberate and
    // it is what makes this a test of the lock rather than of cache coherency:
    // with threads sharing blocks, every hit does an atomic RMW on the same
    // block's `Arc` refcount and the same entry's reference bit, so the cacheline
    // ping-pong dominates and *any* correct implementation measures poorly.
    // Disjoint working sets remove that term and leave the question the defect
    // was about — do independent readers of independent blocks block each other?
    // A single process-wide lock says yes no matter how disjoint the data is.
    // `span` is the same whether one thread runs or four, so a thread's own
    // access pattern and footprint are identical in both measurements and the
    // only thing that changes is how many threads are doing it.
    let span = BLOCKS / THREADS as u64;
    let measure = |threads: usize| {
        let c = Arc::clone(&cache);
        let cached = aggregate_ops_per_sec(threads, OPS, OUT, move |t, i, sink| {
            let idx = (t as u64) * span + i % span;
            let b = c.get(&key(idx)).expect("resident");
            sink.copy_from_slice(&b[..OUT]);
            black_box(&*sink);
        });
        let p = Arc::clone(&plain);
        let control = aggregate_ops_per_sec(threads, OPS, OUT, move |t, i, sink| {
            let idx = (t as u64) * span + i % span;
            let b = &p[idx as usize];
            sink.copy_from_slice(&b[..OUT]);
            black_box(&*sink);
        });
        (cached, control)
    };

    // Best of N on the efficiency ratio. Best-of-N cannot rescue a serialising
    // cache: the serialisation is structural, so it loses every run.
    let mut eff = 0.0f64;
    let mut detail = String::new();
    for _ in 0..ATTEMPTS {
        let (c1, p1) = measure(1);
        let (cn, pn) = measure(THREADS);
        let cache_scaling = cn / c1;
        let control_scaling = pn / p1;
        let e = cache_scaling / control_scaling;
        if e > eff {
            eff = e;
            detail = format!(
                "cache {c1:.0} -> {cn:.0} ops/s ({cache_scaling:.2}x), \
                 no-cache control {p1:.0} -> {pn:.0} ops/s ({control_scaling:.2}x)"
            );
        }
    }

    assert!(
        eff >= GATE,
        "with {THREADS} threads the cache kept only {eff:.2} of the scaling the \
         same work achieves with no cache in the way, on a {cores}-core machine. \
         {detail}. Hits are being serialised on the cache's lock. Measured \
         0.19-0.22 with the single process-wide mutex and 0.58-0.95 with \
         shared-lock sharded hits and per-shard counters; the gate is {GATE}."
    );
}

/// The property the cache exists for, guarded so the cost fixes above cannot be
/// paid for with it. This one is expected to pass before *and* after — it is a
/// regression guard, not a fail-first test — and it is here because "make small
/// reads fast" has an obvious wrong answer: stop caching.
///
/// The leaf sleeps per read, so the ratio is dominated by sleeps rather than by
/// anything about this machine's CPU, and the gate (8x) is far below the 64x the
/// arithmetic predicts.
#[test]
fn repeated_reads_of_a_slow_source_stay_cheap() {
    let _g = perf_lock();
    const BLOCK: u64 = 64 * 1024;
    const READ: usize = 1024;
    const READS: usize = 64; // exactly one block's worth

    struct SlowLeaf {
        reads: AtomicU64,
    }
    impl Provider for SlowLeaf {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                immutable: true,
                slow: true,
                preferred_block: Some(BLOCK as u32),
                ..Capabilities::read_only()
            }
        }
        fn getattr(&self, _p: VPath) -> Result<Option<Stat>, i32> {
            Ok(Some(Stat {
                kind: KIND_FILE,
                size: BLOCK,
                mtime: 3,
            }))
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(Vec::new())
        }
        fn open(&self, _p: VPath, _f: u32) -> Result<(Handle, u64, bool), i32> {
            Ok((1, BLOCK, false))
        }
        fn close(&self, _h: Handle) -> Result<(), i32> {
            Ok(())
        }
        fn read_at(&self, _h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(1));
            let n = buf.len().min((BLOCK - offset.min(BLOCK)) as usize);
            buf[..n].fill(0x5A);
            Ok(n)
        }
    }

    let leaf = Arc::new(SlowLeaf {
        reads: AtomicU64::new(0),
    });
    let uncached: Arc<dyn Provider> = leaf.clone();
    let cache = Arc::new(BlockCache::new(CacheConfig {
        block_size: BLOCK,
        ram_budget: 4 * 1024 * 1024,
        disk_dir: None,
    }));
    let cached = CachingProvider::new(leaf.clone(), cache.clone(), 1);

    let mut buf = vec![0u8; READ];
    let h_raw = uncached.open(VPath::at_default("f"), OPEN_READ).unwrap().0;
    let t0 = Instant::now();
    for i in 0..READS {
        uncached
            .read_at(h_raw, (i * READ) as u64, &mut buf)
            .unwrap();
    }
    let raw = t0.elapsed().as_secs_f64();
    uncached.close(h_raw).unwrap();

    let h = cached.open(VPath::at_default("f"), OPEN_READ).unwrap().0;
    // Warm the single block, then measure the repeat pass — the case the cache
    // exists for: an immutable, high-latency source read repeatedly.
    for i in 0..READS {
        cached.read_at(h, (i * READ) as u64, &mut buf).unwrap();
    }
    let leaf_reads_after_warm = leaf.reads.load(Ordering::Relaxed);
    let t1 = Instant::now();
    for i in 0..READS {
        cached.read_at(h, (i * READ) as u64, &mut buf).unwrap();
    }
    let hot = t1.elapsed().as_secs_f64();
    cached.close(h).unwrap();

    assert_eq!(
        leaf.reads.load(Ordering::Relaxed),
        leaf_reads_after_warm,
        "the repeat pass went back to the slow source — the cache is not caching"
    );
    let speedup = raw / hot.max(1e-9);
    assert!(
        speedup > 8.0,
        "repeated reads of a slow source were only {speedup:.1}x faster than \
         uncached ({raw:.4}s raw vs {hot:.4}s cached over {READS} reads). The \
         point of the cache is that hits are cheap for exactly this case."
    );
}
