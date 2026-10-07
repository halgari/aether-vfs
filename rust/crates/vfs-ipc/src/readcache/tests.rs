use super::*;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, Instant};

const KIB: usize = 1024;

/// A small cache: 16-byte blocks, reads under 8 bytes, 2 blocks a file,
/// 64 bytes in all.
fn tiny() -> ReadCache {
    ReadCache::new(CacheConfig {
        block: 16,
        max_run: 1,
        cold_hits_per_miss: DEFAULT_COLD_HITS_PER_MISS,
        cold_counts_first_fetch: true,
        threshold: 8,
        blocks_per_file: 2,
        max_bytes: 64,
        wait: Duration::from_secs(10),
    })
}

fn content(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
}

/// A fetcher over `data` that counts its calls and their offsets.
struct Source {
    data: Vec<u8>,
    calls: AtomicUsize,
    offsets: Mutex<Vec<u64>>,
}

impl Source {
    fn new(data: Vec<u8>) -> Self {
        Source {
            data,
            calls: AtomicUsize::new(0),
            offsets: Mutex::new(Vec::new()),
        }
    }
    fn fetch(&self, off: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.offsets).push(off);
        let off = off as usize;
        let n = buf.len().min(self.data.len().saturating_sub(off));
        buf[..n].copy_from_slice(&self.data[off..off + n]);
        Ok(n)
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn reg(c: &ReadCache, path: &str, size: usize) -> FileRef {
    c.register(0, path, size as u64, 1, true, false)
}

fn read(c: &ReadCache, f: &FileRef, s: &Source, off: u64, len: usize) -> Option<Vec<u8>> {
    let mut buf = vec![0xEEu8; len];
    let n = c.read(f, off, &mut buf, |o, b| s.fetch(o, b))?;
    buf.truncate(n);
    Some(buf)
}

#[test]
fn a_small_read_is_served_from_one_fetched_block_and_then_from_memory() {
    let c = tiny();
    let s = Source::new(content(100));
    let f = reg(&c, "a", 100);
    assert_eq!(read(&c, &f, &s, 3, 5).unwrap(), &s.data[3..8]);
    assert_eq!(s.calls(), 1);
    assert_eq!(
        *lock(&s.offsets),
        vec![0],
        "the aligned block, not the read"
    );
    for off in 0..12 {
        assert_eq!(
            read(&c, &f, &s, off, 4).unwrap(),
            &s.data[off as usize..off as usize + 4]
        );
    }
    assert_eq!(s.calls(), 1, "every read inside block 0 is a hit");
    let st = c.stats();
    assert_eq!(
        (st.misses, st.hits, st.fetches, st.bytes_fetched),
        (1, 12, 1, 16)
    );
}

#[test]
fn a_read_straddling_two_blocks_fetches_both_and_joins_them() {
    let c = tiny();
    let s = Source::new(content(100));
    let f = reg(&c, "a", 100);
    assert_eq!(read(&c, &f, &s, 13, 7).unwrap(), &s.data[13..20]);
    assert_eq!(*lock(&s.offsets), vec![0, 16]);
    assert_eq!(read(&c, &f, &s, 14, 7).unwrap(), &s.data[14..21]);
    assert_eq!(s.calls(), 2);
}

#[test]
fn the_block_at_end_of_file_is_short_and_reads_are_cut_at_eof() {
    let c = tiny();
    let s = Source::new(content(37));
    let f = reg(&c, "a", 37);
    // Block 2 holds bytes 32..37.
    assert_eq!(
        read(&c, &f, &s, 33, 7).unwrap(),
        &s.data[33..37],
        "cut short at EOF"
    );
    assert_eq!(
        c.stats().bytes_fetched,
        5,
        "a block straddling EOF is short"
    );
    assert_eq!(read(&c, &f, &s, 36, 1).unwrap(), &s.data[36..37]);
    // At and past EOF: not served; the caller's own EOF answer stands.
    assert_eq!(read(&c, &f, &s, 37, 1), None);
    assert_eq!(read(&c, &f, &s, 1000, 1), None);
    // A read ending exactly at EOF across a block boundary.
    assert_eq!(read(&c, &f, &s, 30, 7).unwrap(), &s.data[30..37]);
    // An empty file has nothing to serve.
    let e = reg(&c, "empty", 0);
    assert_eq!(read(&c, &e, &s, 0, 1), None);
}

#[test]
fn large_and_empty_reads_bypass_the_cache() {
    let c = tiny();
    let s = Source::new(content(100));
    let f = reg(&c, "a", 100);
    assert_eq!(read(&c, &f, &s, 0, 8), None, "at the threshold: bypass");
    assert_eq!(read(&c, &f, &s, 0, 0), None, "zero-length: bypass");
    assert_eq!(s.calls(), 0);
    assert!(!c.wants(8) && !c.wants(0) && c.wants(7));
}

#[test]
fn a_fetch_that_comes_back_short_or_fails_is_not_kept() {
    let c = tiny();
    let f = reg(&c, "a", 100);
    let mut buf = [0u8; 4];
    assert_eq!(c.read(&f, 0, &mut buf, |_, b| Ok(b.len() - 1)), None);
    assert_eq!(c.read(&f, 0, &mut buf, |_, _| Err(-5)), None);
    assert_eq!(
        c.stats().resident_bytes,
        0,
        "the reservations were given back"
    );
    let s = Source::new(content(100));
    assert_eq!(
        read(&c, &f, &s, 0, 4).unwrap(),
        &s.data[0..4],
        "and the slot was freed"
    );
}

#[test]
fn a_file_keeps_its_most_recently_used_blocks() {
    let c = tiny(); // two blocks a file
    let s = Source::new(content(100));
    let f = reg(&c, "a", 100);
    read(&c, &f, &s, 0, 1); // block 0
    read(&c, &f, &s, 16, 1); // block 1
    read(&c, &f, &s, 1, 1); // block 0 again: now the most recent
    read(&c, &f, &s, 32, 1); // block 2 evicts block 1
    assert_eq!(s.calls(), 3);
    read(&c, &f, &s, 2, 1);
    assert_eq!(s.calls(), 3, "block 0 stayed");
    read(&c, &f, &s, 17, 1);
    assert_eq!(s.calls(), 4, "block 1 went");
    assert_eq!(c.stats().evictions, 2);
}

#[test]
fn the_process_wide_cap_evicts_the_least_recently_used_block_of_any_file() {
    let c = tiny(); // 64 bytes: four blocks in all
    let s = Source::new(content(100));
    let files: Vec<FileRef> = (0..4).map(|i| reg(&c, &format!("f{i}"), 100)).collect();
    for f in &files {
        read(&c, f, &s, 0, 1);
    }
    assert_eq!(c.stats().resident_bytes, 64);
    read(&c, &files[0], &s, 1, 1); // f0 is now the most recent
    let e = reg(&c, "f4", 100);
    read(&c, &e, &s, 0, 1); // must evict f1, the oldest untouched
    assert_eq!(c.stats().resident_bytes, 64, "never over the cap");
    let before = s.calls();
    read(&c, &files[0], &s, 2, 1);
    read(&c, &files[2], &s, 2, 1);
    read(&c, &files[3], &s, 2, 1);
    assert_eq!(s.calls(), before, "f0, f2, f3 kept their blocks");
    read(&c, &files[1], &s, 2, 1);
    assert_eq!(s.calls(), before + 1, "f1's was the one evicted");
}

#[test]
fn a_block_larger_than_the_whole_budget_is_never_reserved() {
    let c = ReadCache::new(CacheConfig {
        block: 32,
        max_run: 1,
        cold_hits_per_miss: DEFAULT_COLD_HITS_PER_MISS,
        cold_counts_first_fetch: true,
        threshold: 8,
        blocks_per_file: 2,
        max_bytes: 16,
        wait: Duration::from_secs(1),
    });
    let s = Source::new(content(100));
    let f = reg(&c, "a", 100);
    assert_eq!(read(&c, &f, &s, 0, 4), None);
    assert_eq!(s.calls(), 0);
}

#[test]
fn handles_on_one_file_share_its_blocks() {
    let c = tiny();
    let s = Source::new(content(100));
    let a = reg(&c, "data/x.esm", 100);
    let b = reg(&c, "data/x.esm", 100);
    read(&c, &a, &s, 0, 4);
    read(&c, &b, &s, 4, 4);
    assert_eq!(s.calls(), 1);
    // Another root's file of the same name is another file.
    let other = c.register(1, "data/x.esm", 100, 1, true, false);
    read(&c, &other, &s, 0, 4);
    assert_eq!(s.calls(), 2);
}

#[test]
fn two_threads_missing_one_block_fetch_it_once() {
    let c = tiny();
    let f = reg(&c, "a", 100);
    let data = content(100);
    let calls = AtomicUsize::new(0);
    let entered = std::sync::Barrier::new(2);
    let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
    let gate_rx = Mutex::new(gate_rx);
    std::thread::scope(|scope| {
        let (calls, gate_rx, data, entered, c, f) = (&calls, &gate_rx, &data, &entered, &c, &f);
        let fetcher = move |o: u64, b: &mut [u8]| {
            calls.fetch_add(1, Ordering::SeqCst);
            // Hold the fetch until the other reader is waiting on it.
            let _ = lock(gate_rx).recv();
            let o = o as usize;
            b.copy_from_slice(&data[o..o + b.len()]);
            Ok(b.len())
        };
        let first = scope.spawn(move || {
            let mut buf = [0u8; 4];
            entered.wait();
            c.read(f, 0, &mut buf, fetcher).map(|n| buf[..n].to_vec())
        });
        entered.wait();
        // The first thread is inside its fetch once a loading slot exists.
        while !lock(&f.entry.state)
            .slots
            .iter()
            .any(|s| matches!(s.kind, SlotKind::Loading(_)))
        {
            std::thread::yield_now();
        }
        let second = scope.spawn(move || {
            let mut buf = [0u8; 4];
            c.read(f, 8, &mut buf, |_, _| {
                panic!("the second reader must not fetch")
            })
            .map(|n| buf[..n].to_vec())
        });
        std::thread::sleep(Duration::from_millis(50));
        gate_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap().unwrap(), &data[0..4]);
        assert_eq!(second.join().unwrap().unwrap(), &data[8..12]);
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        c.stats().misses,
        2,
        "the waiter counts as a miss, not a hit"
    );
}

#[test]
fn a_failed_fetch_releases_its_waiters_to_read_uncached() {
    let c = tiny();
    let f = reg(&c, "a", 100);
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            let mut buf = [0u8; 4];
            c.read(&f, 0, &mut buf, |_, _| {
                let _ = lock(&go_rx).recv();
                Err(-5)
            })
        });
        while !lock(&f.entry.state)
            .slots
            .iter()
            .any(|s| matches!(s.kind, SlotKind::Loading(_)))
        {
            std::thread::yield_now();
        }
        let second = scope.spawn(|| {
            let mut buf = [0u8; 4];
            c.read(&f, 0, &mut buf, |_, _| panic!("must wait, not fetch"))
        });
        std::thread::sleep(Duration::from_millis(20));
        go_tx.send(()).unwrap();
        assert_eq!(first.join().unwrap(), None);
        assert_eq!(second.join().unwrap(), None);
    });
    assert_eq!(c.stats().resident_bytes, 0);
}

#[test]
fn many_threads_reading_many_files_stay_correct_and_under_the_cap() {
    let c = ReadCache::new(CacheConfig {
        block: 4 * KIB,
        max_run: 1,
        cold_hits_per_miss: DEFAULT_COLD_HITS_PER_MISS,
        cold_counts_first_fetch: true,
        threshold: KIB,
        blocks_per_file: 3,
        max_bytes: 40 * KIB,
        wait: Duration::from_secs(10),
    });
    let data = content(64 * KIB);
    let peak = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for t in 0..8u64 {
            let (c, data, peak) = (&c, &data, &peak);
            scope.spawn(move || {
                let mut x = 0x9E37_79B9_7F4A_7C15u64 ^ t;
                for _ in 0..4000 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    let file = (x % 6) as usize;
                    let f = c.register(0, &format!("f{file}"), data.len() as u64, 1, true, false);
                    let off = (x >> 8) % (data.len() as u64);
                    let len = 1 + ((x >> 40) % 1000) as usize;
                    let mut buf = vec![0u8; len];
                    let got = c.read(&f, off, &mut buf, |o, b| {
                        let o = o as usize;
                        b.copy_from_slice(&data[o..o + b.len()]);
                        Ok(b.len())
                    });
                    if let Some(n) = got {
                        let o = off as usize;
                        assert_eq!(&buf[..n], &data[o..o + n]);
                        assert_eq!(n, len.min(data.len() - o));
                    }
                    peak.fetch_max(c.used.load(Ordering::SeqCst), Ordering::SeqCst);
                }
            });
        }
    });
    assert!(
        peak.load(Ordering::SeqCst) <= 40 * KIB,
        "bounded memory under contention"
    );
    assert!(c.stats().hits > 0);
}

/// A fetch that never returns (its thread was killed mid-fetch, which
/// runs no cleanup) costs at most one reader its deadline: the reader
/// that times out removes it, and the next reader fetches afresh.
#[test]
fn a_fetch_that_never_returns_is_waited_for_once_and_then_replaced() {
    let c = ReadCache::new(CacheConfig {
        wait: Duration::from_millis(200),
        ..tiny().cfg
    });
    let f = reg(&c, "a", 100);
    let data = content(100);
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    std::thread::scope(|scope| {
        // The "dead" fetcher: blocks until the end of the test.
        let stuck = scope.spawn(|| {
            let mut buf = [0u8; 4];
            c.read(&f, 0, &mut buf, |_, _| {
                let _ = lock(&release_rx).recv();
                Err(-5)
            })
        });
        while !lock(&f.entry.state)
            .slots
            .iter()
            .any(|s| matches!(s.kind, SlotKind::Loading(_)))
        {
            std::thread::yield_now();
        }
        // The second reader waits — but only out the fetch's deadline.
        let t = Instant::now();
        let mut buf = [0u8; 4];
        let second = c.read(&f, 0, &mut buf, |_, _| panic!("must wait, not fetch"));
        let waited = t.elapsed();
        assert_eq!(second, None);
        assert!(waited < Duration::from_secs(2), "waited {waited:?}");
        // The third must not wait at all: the dead slot is gone.
        let t = Instant::now();
        let third = c.read(&f, 0, &mut buf, |o, b| {
            let o = o as usize;
            b.copy_from_slice(&data[o..o + b.len()]);
            Ok(b.len())
        });
        let took = t.elapsed();
        assert_eq!(third, Some(4));
        assert_eq!(&buf, &data[0..4]);
        assert!(
            took < Duration::from_millis(100),
            "the third read waited {took:?}"
        );
        release_tx.send(()).unwrap();
        assert_eq!(stuck.join().unwrap(), None, "the late fetch is discarded");
    });
    assert_eq!(c.stats().fetches_abandoned, 1);
    let s = Source::new(content(100));
    assert_eq!(read(&c, &f, &s, 1, 3).unwrap(), &s.data[1..4]);
    assert_eq!(
        s.calls(),
        0,
        "the block the third reader fetched is the one held"
    );
}

/// A reader that finds a fetch already older than its deadline does not
/// wait on it at all: it takes the slot over.
#[test]
fn a_fetch_older_than_its_deadline_is_taken_over_without_waiting() {
    let c = ReadCache::new(CacheConfig {
        wait: Duration::from_millis(50),
        ..tiny().cfg
    });
    let f = reg(&c, "a", 100);
    // A loading slot whose thread is gone: nothing will ever complete it.
    lock(&f.entry.state).slots.push(Slot {
        idx: 0,
        kind: SlotKind::Loading(Arc::new(Flight::new())),
    });
    std::thread::sleep(Duration::from_millis(60));
    let s = Source::new(content(100));
    let t = Instant::now();
    assert_eq!(read(&c, &f, &s, 0, 4).unwrap(), &s.data[0..4]);
    assert!(t.elapsed() < Duration::from_millis(40));
    assert_eq!(s.calls(), 1);
}

/// Fetches that keep failing are counted, and after a few the file goes
/// cold instead of paying a failed block fetch before every read.
#[test]
fn a_file_whose_fetches_keep_failing_goes_cold() {
    let c = tiny();
    let f = reg(&c, "a", 100);
    let calls = AtomicUsize::new(0);
    let mut buf = [0u8; 4];
    for _ in 0..100 {
        let got = c.read(&f, 0, &mut buf, |_, b| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(b.len() - 1)
        });
        assert_eq!(got, None);
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        COLD_AFTER_FAILED_FETCHES as usize
    );
    let st = c.stats();
    assert_eq!(st.fetch_failures, COLD_AFTER_FAILED_FETCHES as u64);
    assert_eq!(st.cold, 1);
    assert_eq!(st.resident_bytes, 0);
}

// ---- runs, units, pressure, diagnostics ----------------------------------

/// A 16-byte-unit cache whose runs grow to 4 units.
fn runs() -> ReadCache {
    ReadCache::new(CacheConfig {
        block: 16,
        max_run: 4,
        threshold: 8,
        blocks_per_file: 64,
        max_bytes: 1 << 20,
        cold_hits_per_miss: DEFAULT_COLD_HITS_PER_MISS,
        cold_counts_first_fetch: true,
        wait: Duration::from_secs(10),
    })
}

#[test]
fn sequential_misses_fetch_growing_runs_and_random_ones_a_single_unit() {
    let c = runs();
    let s = Source::new(content(16 * 64));
    let f = reg(&c, "seq", 16 * 64);
    // A sequential reader: 1-byte reads straight through.
    for off in 0..16 * 32u64 {
        assert_eq!(
            read(&c, &f, &s, off, 1).unwrap(),
            &s.data[off as usize..off as usize + 1]
        );
    }
    // Fetches of 1, 2, 4, 4, 4… units: offsets 0, 16, 48, 112, 176, …
    let offs = lock(&s.offsets).clone();
    assert_eq!(&offs[..5], &[0, 16, 48, 112, 176]);
    assert_eq!(
        c.stats().fetches,
        2 + 8,
        "32 units in runs of 1, 2, then 4s"
    );
    let before = c.stats().bytes_fetched;
    // A random reader starts every run over at one unit.
    let r = reg(&c, "rand", 16 * 64);
    let s2 = Source::new(content(16 * 64));
    for idx in [40u64, 3, 60, 17] {
        read(&c, &r, &s2, idx * 16 + 5, 1).unwrap();
    }
    assert_eq!(c.stats().bytes_fetched - before, 4 * 16, "one unit each");
}

#[test]
fn a_run_stops_at_end_of_file_and_at_a_unit_already_held() {
    let c = runs();
    let s = Source::new(content(16 * 5 + 3));
    let f = reg(&c, "f", 16 * 5 + 3);
    read(&c, &f, &s, 48, 1).unwrap(); // unit 3 alone
    read(&c, &f, &s, 0, 1).unwrap(); // unit 0
    read(&c, &f, &s, 16, 1).unwrap(); // sequential: units 1, 2 — not 3, held
    assert_eq!(*lock(&s.offsets), vec![48, 0, 16]);
    assert_eq!(c.stats().bytes_fetched, 16 + 16 + 32);
    read(&c, &f, &s, 64, 1).unwrap(); // unit 4: not where the last run ended
    assert_eq!(c.stats().bytes_fetched, 16 + 16 + 32 + 16);
    read(&c, &f, &s, 81, 1).unwrap(); // sequential, but only 3 bytes are left
    assert_eq!(
        c.stats().bytes_fetched,
        16 + 16 + 32 + 16 + 3,
        "never past EOF"
    );
    for off in 0..16 * 5 + 3u64 {
        assert_eq!(
            read(&c, &f, &s, off, 1).unwrap(),
            &s.data[off as usize..off as usize + 1]
        );
    }
    assert_eq!(s.calls(), 5);
}

/// A small file costs its size, never a whole unit, with the defaults.
#[test]
fn a_unit_is_never_larger_than_its_file() {
    let c = ReadCache::default();
    let s = Source::new(content(4096));
    let f = reg(&c, "a.json", 4096);
    for off in (0..4096u64).step_by(100) {
        assert!(read(&c, &f, &s, off, 1).is_some());
    }
    let st = c.stats();
    assert_eq!(
        (st.fetches, st.bytes_fetched, st.resident_bytes),
        (1, 4096, 4096)
    );
}

#[test]
fn a_waiter_on_a_run_gets_its_own_unit() {
    let c = runs();
    let f = reg(&c, "f", 16 * 8);
    let data = content(16 * 8);
    let mut buf = [0u8; 1];
    // Make the next miss a 2-unit run starting at unit 1.
    c.read(&f, 0, &mut buf, |o, b| {
        b.copy_from_slice(&data[o as usize..o as usize + b.len()]);
        Ok(b.len())
    })
    .unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let rx = Mutex::new(rx);
    std::thread::scope(|scope| {
        let (c, f, data, rx) = (&c, &f, &data, &rx);
        let loader = scope.spawn(move || {
            let mut b1 = [0u8; 1];
            c.read(f, 16, &mut b1, |o, b| {
                let _ = lock(rx).recv();
                b.copy_from_slice(&data[o as usize..o as usize + b.len()]);
                Ok(b.len())
            })
            .map(|_| b1[0])
        });
        while lock(&f.entry.state)
            .slots
            .iter()
            .filter(|s| matches!(s.kind, SlotKind::Loading(_)))
            .count()
            < 2
        {
            std::thread::yield_now();
        }
        let waiter = scope.spawn(move || {
            let mut b2 = [0u8; 3];
            c.read(f, 37, &mut b2, |_, _| panic!("unit 2 is in the run"))
                .map(|_| b2)
        });
        std::thread::sleep(Duration::from_millis(20));
        tx.send(()).unwrap();
        assert_eq!(loader.join().unwrap(), Some(data[16]));
        assert_eq!(waiter.join().unwrap().unwrap(), data[37..40]);
    });
}

/// A miss on a unit the process-wide cap evicted is a capacity miss: it
/// is counted as such and never sends the file cold.
#[test]
fn misses_caused_by_the_global_cap_do_not_count_against_locality() {
    let c = ReadCache::new(CacheConfig {
        block: 16,
        max_run: 1,
        threshold: 8,
        blocks_per_file: 64,
        max_bytes: 32,
        cold_hits_per_miss: 8,
        cold_counts_first_fetch: true,
        wait: Duration::from_secs(1),
    });
    let s = Source::new(content(16 * 40));
    let a = reg(&c, "a", 16 * 40);
    // `a` reads unit 0 and other files push it out, over and over: every
    // miss of `a` after the first is the cap's doing.
    for i in 0..100u64 {
        read(&c, &a, &s, i % 16, 1).unwrap();
        for k in 0..2 {
            let other = reg(&c, &format!("b{i}-{k}"), 16 * 40);
            read(&c, &other, &s, 0, 1).unwrap();
        }
    }
    let top = c.top_files(10);
    let da = top.iter().find(|r| r.path == "a").unwrap();
    assert!(da.diag.pressure_misses >= 90, "{da:?}");
    assert_eq!(
        da.diag.cold_guard, 0,
        "`a` reads one unit: not poor locality"
    );
    assert!(!da.cold_now);
    assert!(c.stats().pressure_misses >= 90);
}

#[test]
fn the_per_file_table_names_the_busiest_files_and_why_they_went_cold() {
    let c = tiny(); // 2 units a file, guard at 8 hits a miss
    let s = Source::new(content(16 * 64));
    let busy = reg(&c, "data/skyrim.esm", 16 * 64);
    for off in 0..200u64 {
        read(&c, &busy, &s, off % 16, 1).unwrap();
    }
    let rand = reg(&c, "data/random.bsa", 16 * 64);
    for i in 0..20u64 {
        let _ = read(&c, &rand, &s, (i * 7 % 64) * 16, 1);
    }
    let bad = reg(&c, "data/broken.dds", 100);
    let mut buf = [0u8; 2];
    for _ in 0..COLD_AFTER_FAILED_FETCHES {
        let _ = c.read(&bad, 0, &mut buf, |_, _| Err(-5));
    }
    let top = c.top_files(2);
    assert_eq!(top.len(), 2);
    assert_eq!(top[0].path, "data/skyrim.esm");
    assert_eq!(
        (
            top[0].diag.reads,
            top[0].diag.hits,
            top[0].diag.misses,
            top[0].diag.fetches
        ),
        (200, 199, 1, 1)
    );
    assert_eq!(top[0].diag.bytes_fetched, 16);
    assert_eq!(top[1].path, "data/random.bsa");
    assert_eq!(top[1].diag.cold_guard, 1);
    assert!(top[1].cold_now);
    let all = c.top_files(10);
    let b = all.iter().find(|r| r.path == "data/broken.dds").unwrap();
    assert_eq!((b.diag.cold_failures, b.diag.cold_guard), (1, 0));
}

#[test]
fn a_swept_files_diagnostics_stay_in_the_table() {
    let c = tiny();
    let s = Source::new(content(100));
    {
        let f = reg(&c, "gone.txt", 3);
        read(&c, &f, &s, 0, 2).unwrap();
        c.invalidate_path(0, "unrelated"); // nothing to do with it
    }
    // Evict its unit, then sweep it out with many other registrations.
    for i in 0..8 {
        let f = reg(&c, &format!("big{i}"), 100);
        read(&c, &f, &s, 0, 1).unwrap();
    }
    for i in 0..SWEEP_MIN + 10 {
        let _ = reg(&c, &format!("x{i}"), 100);
    }
    assert!(!lock(&c.files).by_name.contains_key(&Name {
        root: 0,
        path: "gone.txt".into()
    }));
    let top = c.top_files(100);
    let g = top
        .iter()
        .find(|r| r.path == "gone.txt")
        .expect("retired diag kept");
    assert_eq!(
        (g.diag.reads, g.diag.fetches, g.diag.bytes_fetched),
        (1, 1, 3)
    );
}

/// With the defaults, random 4 KiB reads inside a region the file may
/// hold warm up and stay cached — first fetches are not held against
/// it — while the same reads over a file far larger than it may hold
/// keep re-fetching what its own LRU dropped, and go cold.
#[test]
fn a_warming_region_stays_cached_and_a_file_read_at_random_goes_cold() {
    let mib = 1u64 << 20;
    let c = ReadCache::default();
    let region = reg(&c, "region.esm", (64 * mib) as usize);
    let wide = reg(&c, "wide.bsa", (64 * mib) as usize);
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut buf = vec![0u8; 4096];
    let mut fetch = |_: u64, b: &mut [u8]| Ok(b.len());
    for _ in 0..20_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let off = 16 * mib + x % (8 * mib - 4096);
        assert!(c.read(&region, off, &mut buf, &mut fetch).is_some());
    }
    for _ in 0..20_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let _ = c.read(&wide, x % (64 * mib - 4096), &mut buf, &mut fetch);
    }
    let top = c.top_files(2);
    let r = top.iter().find(|r| r.path == "region.esm").unwrap();
    assert_eq!(r.diag.cold_guard, 0, "{r:?}");
    assert!(r.diag.hits > 19_000, "{r:?}");
    let w = top.iter().find(|r| r.path == "wide.bsa").unwrap();
    assert!(w.diag.cold_guard >= 1, "{w:?}");
}

#[test]
fn the_defaults_are_64k_units_runs_to_1mib_and_256mib() {
    let d = CacheConfig::default();
    assert_eq!((d.block, d.max_run, d.threshold), (64 << 10, 16, 64 << 10));
    assert_eq!((d.blocks_per_file, d.max_bytes), (256, 256 << 20));
    assert_eq!(ReadCache::default().stats().max_bytes, 256 << 20);
}

// ---- coherence ---------------------------------------------------------

#[test]
fn a_write_open_by_any_handle_drops_the_file_and_it_is_never_cached_again() {
    let c = tiny();
    let s = Source::new(content(100));
    let r = reg(&c, "data/a.ini", 100);
    read(&c, &r, &s, 0, 4).unwrap();
    assert_eq!(c.stats().resident_bytes, 16);
    let w = c.register(0, "data/a.ini", 100, 1, false, true);
    assert!(
        !w.cacheable(),
        "a write handle is never served from the cache"
    );
    assert_eq!(c.stats().resident_bytes, 0, "the blocks went");
    assert_eq!(
        read(&c, &r, &s, 0, 4),
        None,
        "the earlier handle reads uncached now"
    );
    let again = reg(&c, "data/a.ini", 100);
    assert!(!again.cacheable(), "and so does every later open");
    let st = c.stats();
    assert_eq!((st.invalidations, st.blocks_invalidated), (1, 1));
}

#[test]
fn a_file_the_director_calls_mutable_is_never_cached() {
    let c = tiny();
    let s = Source::new(content(100));
    let m = c.register(0, "saves/a.ess", 100, 1, false, false);
    assert!(!m.cacheable());
    assert_eq!(read(&c, &m, &s, 0, 4), None);
    assert_eq!(s.calls(), 0);
    // An immutable open of a path once served mutable is not believed.
    let r = c.register(0, "saves/a.ess", 100, 1, true, false);
    assert!(!r.cacheable());
    // And one served mutable after being cached drops the cached copy.
    let x = reg(&c, "data/x", 100);
    read(&c, &x, &s, 0, 4).unwrap();
    let _ = c.register(0, "data/x", 100, 1, false, false);
    assert_eq!(read(&c, &x, &s, 0, 4), None);
    assert_eq!(c.stats().resident_bytes, 0);
}

#[test]
fn a_write_or_truncate_through_another_handle_invalidates() {
    let c = tiny();
    let s = Source::new(content(100));
    let r = reg(&c, "a", 100);
    let other = reg(&c, "a", 100);
    read(&c, &r, &s, 0, 4).unwrap();
    c.invalidate(&other);
    assert_eq!(read(&c, &r, &s, 0, 4), None);
    assert_eq!(c.stats().resident_bytes, 0);
}

#[test]
fn a_delete_or_rename_drops_the_path_and_everything_under_it() {
    let c = tiny();
    let s = Source::new(content(100));
    let a = reg(&c, "data/a.esp", 100);
    let ab = reg(&c, "data/ab.esp", 100);
    let sub = reg(&c, "data/sub/x.dds", 100);
    let other = c.register(1, "data/sub/x.dds", 100, 1, true, false);
    for f in [&a, &ab, &sub, &other] {
        read(&c, f, &s, 0, 4).unwrap();
    }
    c.invalidate_path(0, "data/sub");
    assert_eq!(read(&c, &sub, &s, 0, 4), None, "under the path");
    assert!(read(&c, &a, &s, 0, 4).is_some(), "beside it");
    assert!(read(&c, &other, &s, 0, 4).is_some(), "another root");
    c.invalidate_path(0, "data/a.esp");
    assert_eq!(read(&c, &a, &s, 0, 4), None);
    assert!(
        read(&c, &ab, &s, 0, 4).is_some(),
        "a sibling sharing a prefix"
    );
    // A rename's target, not opened before, is not believed afterwards.
    c.invalidate_path(0, "data/new.esp");
    assert!(!reg(&c, "data/new.esp", 100).cacheable());
    // The whole root.
    c.invalidate_path(0, ".");
    assert_eq!(read(&c, &ab, &s, 0, 4), None);
}

#[test]
fn a_remount_with_other_content_at_the_path_is_another_file() {
    let c = tiny();
    let old = Source::new(content(100));
    let new = Source::new(content(100).into_iter().map(|b| !b).collect());
    let a = c.register(0, "a", 100, 1, true, false);
    read(&c, &a, &old, 0, 4).unwrap();
    let b = c.register(0, "a", 100, 2, true, false);
    assert_eq!(read(&c, &b, &new, 0, 4).unwrap(), &new.data[0..4]);
    assert_eq!(
        read(&c, &a, &old, 0, 4),
        None,
        "the old handle reads uncached"
    );
    // A different size is a different version too.
    let d = c.register(0, "a", 99, 2, true, false);
    assert_eq!(read(&c, &d, &new, 0, 4).unwrap(), &new.data[0..4]);
}

#[test]
fn a_fetch_overtaken_by_an_invalidation_is_not_installed() {
    let c = tiny();
    let f = reg(&c, "a", 100);
    let data = content(100);
    let got = {
        let mut buf = [0u8; 4];
        c.read(&f, 0, &mut buf, |o, b| {
            c.invalidate(&f); // a write lands while the block is in flight
            let o = o as usize;
            b.copy_from_slice(&data[o..o + b.len()]);
            Ok(b.len())
        })
    };
    assert_eq!(got, None, "served uncached instead");
    assert_eq!(c.stats().resident_bytes, 0);
    assert!(lock(&f.entry.state).slots.is_empty());
}

// ---- locality -----------------------------------------------------------

#[test]
fn a_file_read_at_random_goes_cold_and_is_read_uncached_for_a_while() {
    let c = ReadCache::new(CacheConfig {
        block: 16,
        max_run: 1,
        cold_hits_per_miss: DEFAULT_COLD_HITS_PER_MISS,
        cold_counts_first_fetch: true,
        threshold: 8,
        blocks_per_file: 2,
        max_bytes: 1 << 20,
        wait: Duration::from_secs(1),
    });
    let s = Source::new(content(16 * 64));
    let f = reg(&c, "big", 16 * 64);
    // Every read a new block: all misses.
    for i in 0..COLD_AFTER_MISSES as u64 {
        assert!(read(&c, &f, &s, i * 16 * 3 % (16 * 64), 1).is_some());
    }
    assert_eq!(c.stats().cold, 1);
    assert_eq!(c.stats().resident_bytes, 0, "a cold file holds nothing");
    let calls = s.calls();
    for _ in 0..COLD_READS {
        assert_eq!(read(&c, &f, &s, 0, 1), None);
    }
    assert_eq!(s.calls(), calls);
    assert!(
        read(&c, &f, &s, 0, 1).is_some(),
        "tried again after the spell"
    );
}

#[test]
fn a_file_with_good_locality_never_goes_cold() {
    let c = tiny();
    let s = Source::new(content(16 * 100));
    let f = reg(&c, "seq", 16 * 100);
    // 1-byte sequential reads: one miss per 16 reads.
    for off in 0..16 * 100u64 {
        assert!(read(&c, &f, &s, off, 1).is_some());
    }
    assert_eq!(c.stats().cold, 0);
    assert_eq!(s.calls(), 100);
}

#[test]
fn unused_entries_are_swept_but_poisoned_and_held_ones_are_not() {
    let c = tiny();
    let s = Source::new(content(100));
    let held = reg(&c, "held", 100);
    let _ = c.register(0, "written", 100, 1, false, true);
    let cached = reg(&c, "cached", 100);
    read(&c, &cached, &s, 0, 1).unwrap();
    drop(cached);
    for i in 0..SWEEP_MIN + 10 {
        let _ = reg(&c, &format!("x{i}"), 100);
    }
    let reg_ = lock(&c.files);
    let has = |p: &str| {
        reg_.by_name.contains_key(&Name {
            root: 0,
            path: p.to_string(),
        })
    };
    assert!(has("held") && has("written") && has("cached"));
    assert!(reg_.by_name.len() < SWEEP_MIN, "the unused ones went");
    drop(reg_);
    drop(held);
}
