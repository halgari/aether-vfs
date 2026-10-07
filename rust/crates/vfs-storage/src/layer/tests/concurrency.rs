//! Readers, writers and cold fills running together.

use super::*;

#[test]
fn a_read_fill_before_a_write_never_serves_the_old_block() {
    // Rule 4 (the RAM tier's stale-refill race): a read that fills the RAM
    // tier with a block, then a write and commit of that block, then a read.
    // Every fill and every commit of a layer file happens under the file's
    // state lock, so the only order the race can take is this one; the read
    // after the commit must see the new bytes, from RAM or the store.
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    write_file(&p, "r.bin", 0, &vec![1u8; 2 * BS as usize]);
    let (h, _, _) = p.open(at("r.bin"), OPEN_WRITE).unwrap();
    assert_eq!(read_range(&p, h, 0, 4), [1, 1, 1, 1]); // fills block 0 into RAM
    let hits = s.ram.stats().hits;
    p.write_at(h, 0, &[2u8; 4]).unwrap();
    p.flush(h).unwrap();
    let (r, _, _) = p.open(at("r.bin"), OPEN_READ).unwrap();
    assert_eq!(read_range(&p, r, 0, 5), [2, 2, 2, 2, 1]);
    assert!(
        s.ram.stats().hits > hits,
        "the committed block is served from RAM"
    );
    p.close(r).unwrap();
    p.close(h).unwrap();
}

/// Installs a gate as the storage's layer read hook: every read that
/// enters waits there — holding the file's state lock — until `want`
/// reads are inside at once. Reads that exclude each other never get
/// there; the first to wait `patience` gives up for all of them, so the
/// caller's assertion on the returned peak fails instead of the test
/// hanging. Returns the gate: `(inside, peak, gave_up)`.
#[allow(clippy::type_complexity)]
pub(super) fn gate_reads(
    s: &Storage,
    want: usize,
    patience: Duration,
) -> Arc<(Mutex<(usize, usize, bool)>, Condvar)> {
    let gate = Arc::new((Mutex::new((0usize, 0usize, false)), Condvar::new()));
    let g = Arc::clone(&gate);
    *s.layer_read_hook.lock().unwrap() = Some(Arc::new(move || {
        let (m, cv) = &*g;
        let mut st = m.lock().unwrap();
        st.0 += 1;
        st.1 = st.1.max(st.0);
        cv.notify_all();
        let (mut st, timeout) = cv
            .wait_timeout_while(st, patience, |st| st.1 < want && !st.2)
            .unwrap();
        if timeout.timed_out() {
            st.2 = true;
            cv.notify_all();
        }
        st.0 -= 1;
    }));
    gate
}

#[test]
fn reads_of_one_file_run_concurrently() {
    // The game issues eight 1 MiB reads of one plugin at once. Each read
    // holds the file's state lock shared, so all eight are inside the
    // read together — on one handle or several.
    const N: usize = 8;
    let (s, _d) = temp_storage();
    let p = s.layer("l").unwrap();
    let body: Vec<u8> = (0..N * BS as usize)
        .map(|i| (i / BS as usize) as u8 + 1)
        .collect();
    write_file(&p, "big.bin", 0, &body);
    let (h1, _, _) = p.open(at("big.bin"), OPEN_READ).unwrap();
    let (h2, _, _) = p.open(at("big.bin"), OPEN_READ).unwrap();

    let gate = gate_reads(&s, N, Duration::from_secs(5));
    std::thread::scope(|sc| {
        for i in 0..N {
            let p = &p;
            sc.spawn(move || {
                let h = if i % 2 == 0 { h1 } else { h2 };
                let mut buf = vec![0u8; BS as usize];
                let n = p.read_at(h, i as u64 * BS, &mut buf).unwrap();
                assert_eq!(n, BS as usize);
                assert!(buf.iter().all(|&b| b == i as u8 + 1), "block {i}");
            });
        }
    });
    *s.layer_read_hook.lock().unwrap() = None;
    let (_, peak, gave_up) = *gate.0.lock().unwrap();
    assert!(!gave_up, "reads of one file excluded each other");
    assert_eq!(peak, N, "all {N} reads were inside the read at once");
    p.close(h1).unwrap();
    p.close(h2).unwrap();
}

/// A hook that parks every call until [`Park::release`]: installed as
/// one of the storage's layer hooks, it holds reads at that point.
#[derive(Default)]
pub(super) struct Park {
    /// (calls parked or past, released)
    st: Mutex<(usize, bool)>,
    cv: Condvar,
}

impl Park {
    fn hook(self: &Arc<Self>) -> Arc<dyn Fn() + Send + Sync> {
        let park = Arc::clone(self);
        Arc::new(move || {
            let mut st = park.st.lock().unwrap();
            st.0 += 1;
            park.cv.notify_all();
            let _released = park.cv.wait_while(st, |st| !st.1).unwrap();
        })
    }

    /// Waits until `n` calls are parked; false if they are not within
    /// `patience` (calls that exclude each other never all arrive).
    fn wait_for(&self, n: usize, patience: Duration) -> bool {
        let (_st, timeout) = self
            .cv
            .wait_timeout_while(self.st.lock().unwrap(), patience, |st| st.0 < n)
            .unwrap();
        !timeout.timed_out()
    }

    fn release(&self) {
        self.st.lock().unwrap().1 = true;
        self.cv.notify_all();
    }
}

#[test]
fn a_change_waits_for_a_read_in_flight() {
    // A read holds the state shared; everything that changes the file
    // takes it exclusive, so none of it lands while a read is inside —
    // the read returns the bytes from before the change.
    type Change = fn(&Arc<dyn Provider>, u64);
    let changes: [(&str, Change, &[u8]); 6] = [
        (
            "write_at",
            |p, w| {
                p.write_at(w, 0, &[2u8; 4]).unwrap();
            },
            &[2, 2, 2, 2, 1, 1],
        ),
        ("set_len", |p, w| p.set_len(w, 3).unwrap(), &[1, 1, 1]),
        ("flush", |p, w| p.flush(w).unwrap(), &[1, 1, 1, 1, 1, 1]),
        // A close commits the handle's unflushed byte.
        ("close", |p, w| p.close(w).unwrap(), &[1, 1, 1, 1, 1, 1]),
        (
            "open with OPEN_TRUNC",
            |p, _| {
                let (t, _, _) = p.open(at("f.bin"), OPEN_WRITE | OPEN_TRUNC).unwrap();
                p.close(t).unwrap();
            },
            &[],
        ),
        (
            "set_attr size",
            |p, _| {
                let attr = SetAttr {
                    size: Some(2),
                    mtime: None,
                };
                p.set_attr(at("f.bin"), attr).unwrap();
            },
            &[1, 1],
        ),
    ];
    for (what, change, after) in changes {
        let (s, _d) = temp_storage();
        let p = s.layer("l").unwrap();
        write_file(&p, "f.bin", 0, &[1u8; 6]);
        let (w, _, _) = p.open(at("f.bin"), OPEN_WRITE).unwrap();
        let (r, _, _) = p.open(at("f.bin"), OPEN_READ).unwrap();
        // Unflushed bytes, so the flush and the close have a commit to run.
        p.write_at(w, 5, &[1u8]).unwrap();

        // The reader parks inside the read until released.
        let park = Arc::new(Park::default());
        *s.layer_read_hook.lock().unwrap() = Some(park.hook());
        let changed = AtomicBool::new(false);
        let (early, read) = std::thread::scope(|sc| {
            let reader = sc.spawn(|| {
                let mut buf = [0u8; 6];
                let n = p.read_at(r, 0, &mut buf).unwrap();
                buf[..n].to_vec()
            });
            let parked = park.wait_for(1, Duration::from_secs(5));
            // Later reads (the checks below) must not park.
            *s.layer_read_hook.lock().unwrap() = None;
            sc.spawn(|| {
                change(&p, w);
                changed.store(true, Ordering::SeqCst);
            });
            // The change cannot finish while the read is parked. (A sleep
            // can only miss a change that wrongly got through late, never
            // fail a correct run.) The reader is released before anything
            // is asserted, so a failure fails the test instead of hanging
            // the scope on a reader nobody releases.
            std::thread::sleep(Duration::from_millis(100));
            let early = changed.load(Ordering::SeqCst);
            park.release();
            assert!(parked, "{what}: the read never reached the hook");
            (early, reader.join().unwrap())
        });
        assert!(!early, "{what} landed while a read held the file's state");
        assert_eq!(read, [1u8; 6], "{what}: the read");
        assert!(changed.load(Ordering::SeqCst));
        assert_eq!(read_range(&p, r, 0, 16), after, "after {what}");
        p.close(r).unwrap();
        if what != "close" {
            p.close(w).unwrap();
        }
    }
}

#[test]
fn concurrent_cold_fills_of_one_block_finish_before_a_commit() {
    // The case the shared lock exists for, and the one it must not get
    // wrong: several reads miss the RAM tier on the same block and fill
    // it from the store at once, with a writer waiting behind them.
    //
    // Every reader is held between its store read and its put into the
    // tier, so all N hold the block's *old* bytes, under the shared lock,
    // at the same moment — by construction, not by timing. The commit
    // cannot start until each has put; it then drops their blocks and
    // puts its own. If a fill ran outside the lock, or a commit did not
    // exclude readers, the commit would land while the readers are held
    // and their puts would leave the old block in the tier behind it.
    const N: usize = 4;
    let (s, d) = temp_storage();
    let p = s.layer("l").unwrap();
    let old: Vec<u8> = (0..2 * BS as usize).map(|i| (i % 251) as u8).collect();
    write_file(&p, "c.bin", 0, &old);
    // Reopen: the tier is cold, so every read below fills from the store.
    drop(p);
    drop(s);
    let s = Storage::open(d.path(), cfg()).unwrap();
    let p = s.layer("l").unwrap();
    let lid = s.catalog.layer_id("l").unwrap().unwrap();
    let id = layer_file_id(&s.catalog.get(lid, "c.bin").unwrap().unwrap().guid);
    assert!(s.ram.get(&id, 0).is_none(), "the tier starts cold");
    let misses = s.ram.stats().misses;

    let (r, _, _) = p.open(at("c.bin"), OPEN_READ).unwrap();
    let (w, _, _) = p.open(at("c.bin"), OPEN_WRITE).unwrap();
    let park = Arc::new(Park::default());
    *s.layer_fill_hook.lock().unwrap() = Some(park.hook());
    let changed = AtomicBool::new(false);
    let new = vec![7u8; BS as usize];
    let (all_filling, early, reads) = std::thread::scope(|sc| {
        let readers: Vec<_> = (0..N)
            .map(|_| {
                sc.spawn(|| {
                    let mut buf = vec![0u8; BS as usize];
                    let n = p.read_at(r, 0, &mut buf).unwrap();
                    buf.truncate(n);
                    buf
                })
            })
            .collect();
        let all_filling = park.wait_for(N, Duration::from_secs(5));
        // The writer's own load of the block must not park.
        *s.layer_fill_hook.lock().unwrap() = None;
        sc.spawn(|| {
            p.write_at(w, 0, &new).unwrap();
            p.flush(w).unwrap();
            changed.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(100));
        let early = changed.load(Ordering::SeqCst);
        park.release();
        let reads: Vec<Vec<u8>> = readers.into_iter().map(|h| h.join().unwrap()).collect();
        (all_filling, early, reads)
    });
    assert!(all_filling, "{N} fills of one block never overlapped");
    assert!(!early, "the commit landed while readers were mid-fill");
    for got in &reads {
        assert_eq!(got[..], old[..BS as usize], "a reader's bytes");
    }
    // Each reader missed the tier and went to the store; the writer's
    // load of the block was a hit on what they filled.
    assert_eq!(s.ram.stats().misses - misses, N as u64);
    assert!(changed.load(Ordering::SeqCst));
    // After the commit the tier holds the new block, not a reader's.
    assert_eq!(s.ram.get(&id, 0).expect("the committed block")[..], new[..]);
    assert_eq!(read_range(&p, r, 0, BS as usize), new);
    assert_eq!(
        read_range(&p, r, BS, BS as usize)[..],
        old[BS as usize..],
        "the block the write did not touch"
    );
    p.close(r).unwrap();
    p.close(w).unwrap();
}

#[test]
fn concurrent_reads_see_whole_changes_and_never_an_older_one() {
    // Readers race a writer that rewrites, truncates, flushes and
    // reopens one file. Every version is one byte value repeated, so a
    // read that saw part of a change shows two values; and a read that
    // began after a change completed must not see an older version (a
    // reader's stale RAM-tier refill landing after a commit would).
    //
    // Run three ways. With the default tier every read is a RAM hit on
    // the blocks the last commit put, so that run only covers the state
    // lock. A tier of two blocks holds a third of the file: readers
    // miss, fill from the store and evict each other constantly, which
    // is where a stale refill could happen. With no tier every read goes
    // to the store.
    const LEN: usize = 5 * BS as usize + 100; // 6 dirty blocks: write_at commits
    const SHORT: usize = BS as usize / 2;
    const LAST: u8 = 150;
    for ram_tier_bytes in [cfg().ram_tier_bytes, 2 * BS, 0] {
        let d = tempfile::tempdir().unwrap();
        let s = Storage::open(
            d.path(),
            StorageConfig {
                ram_tier_bytes,
                ..cfg()
            },
        )
        .unwrap();
        let p = s.layer("l").unwrap();
        write_file(&p, "v.bin", 0, &vec![1u8; LEN]);
        let evicts_before = s.ram.stats().evicts;
        // The newest version whose write has completed.
        let floor = AtomicU8::new(1);
        let done = AtomicBool::new(false);
        std::thread::scope(|sc| {
            for t in 0..6u64 {
                let (p, floor, done) = (&p, &floor, &done);
                sc.spawn(move || {
                    let (h, _, _) = p.open(at("v.bin"), OPEN_READ).unwrap();
                    // Readers start at different offsets: 0, mid-block and
                    // a block boundary, the last two past the truncated
                    // length.
                    let off = [0, 0, BS + 10, BS + 10, 2 * BS, 0][t as usize];
                    let mut buf = vec![0u8; LEN + BS as usize];
                    let mut reads = 0u64;
                    while !done.load(Ordering::SeqCst) || reads < 50 {
                        let before = floor.load(Ordering::SeqCst);
                        let n = p.read_at(h, off, &mut buf).unwrap();
                        reads += 1;
                        let full = LEN - off as usize;
                        let short = SHORT.saturating_sub(off as usize);
                        assert!(n == full || n == short, "read of {n} bytes at {off}");
                        if n == 0 {
                            continue;
                        }
                        let v = buf[0];
                        assert!(
                            buf[..n].iter().all(|&b| b == v),
                            "a read at {off} saw two versions (tier {ram_tier_bytes})"
                        );
                        assert!(
                            v >= before,
                            "read version {v} after {before} completed (tier {ram_tier_bytes})"
                        );
                    }
                    p.close(h).unwrap();
                });
            }
            let (mut w, _, _) = p.open(at("v.bin"), OPEN_WRITE).unwrap();
            for v in 2..=LAST {
                // Shrink first, so the rewrite below never leaves old
                // bytes past a shorter length; a reader in between sees
                // the old version cut short, which is a whole state.
                if v % 3 == 0 {
                    p.set_len(w, SHORT as u64).unwrap();
                }
                // One call replaces the whole file: exclusive, so atomic
                // to readers. From a truncated file it also regrows it.
                let len = if v % 5 == 0 { SHORT } else { LEN };
                if len == SHORT {
                    // A short rewrite of a long file would keep the old
                    // tail.
                    p.set_len(w, SHORT as u64).unwrap();
                }
                p.write_at(w, 0, &vec![v; len]).unwrap();
                floor.store(v, Ordering::SeqCst);
                if v % 4 == 0 {
                    p.flush(w).unwrap();
                }
                if v % 7 == 0 {
                    p.close(w).unwrap();
                    w = p.open(at("v.bin"), OPEN_WRITE).unwrap().0;
                }
            }
            p.close(w).unwrap();
            done.store(true, Ordering::SeqCst);
        });
        assert_eq!(read_file(&p, "v.bin"), vec![LAST; SHORT]);
        // The small tier really was filled and evicted by the run (so
        // readers did refill from the store), and the default one never
        // had to evict.
        let evicted = s.ram.stats().evicts - evicts_before;
        match ram_tier_bytes {
            0 => assert_eq!(s.ram.stats().blocks, 0, "no tier holds nothing"),
            b if b == 2 * BS => assert!(evicted > 100, "tier of 2 blocks: {evicted} evictions"),
            _ => assert_eq!(evicted, 0, "the default tier never evicts here"),
        }
    }
}
