//! `ring-bench`: what a game thread pays for a file operation over the ring,
//! alone and beside other threads — one of which is slow.
//!
//! Native Linux on both ends: the real `Director` behind
//! `IpcServe::start_file_backed_with_workers`, driven by the ring client the
//! shim uses. The shim's hook work (path decode, handle tables) is not in it;
//! the ring, the workers, the bulk arena and the client's own concurrency
//! control are.
//!
//! Two client disciplines, so one binary gives the before and the after:
//!
//! - `locked` — one process-wide lock held across every round trip. This is
//!   what `FuseClient` did while it carried `ring_lock`.
//! - `gated` — no lock; `vfs_ipc::DataGate` bounds reads in flight below the
//!   worker count and `vfs_ipc::read_fragmented` does the read. This is what
//!   `FuseClient` does now, by calling the same two things.
//!
//! Usage: `ring-bench <scratch-dir> [locked|gated|both] [workers] [seconds]`
//!
//! The scratch directory holds the ring file, so pointing it at a tmpfs
//! (`/dev/shm/...`) or at a disk filesystem measures that choice too.

#[cfg(unix)]
mod imp {
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use vfs_director::{Director, IpcServe};
    use vfs_ipc::{DataGate, Notifier, ReadPlan, RingClient, SharedSeg, SpinNotifier};
    use vfs_protocol as P;
    use vfs_provider::{
        Access, Capabilities, CaseMatch, DirEntry, Handle, Provider, RootId, Stat, VPath, KIND_DIR,
        KIND_FILE,
    };

    const FAST: &str = "data/fast.bin";
    const SLOW: &str = "data/slow.bin";
    const FILE_LEN: u64 = 64 << 20;
    /// What one read of the slow file costs the worker that serves it: a
    /// stand-in for a block fetched from the network.
    const SLOW_READ: Duration = Duration::from_millis(200);
    /// How long the slow requester stays off the ring between its reads.
    const SLOW_GAP: Duration = Duration::from_millis(5);
    const PAYLOAD_CAP: u32 = 1_048_576;

    /// Two files of pseudo-random bytes, one of which sleeps on every read.
    struct Mem {
        data: Vec<u8>,
        opens: Mutex<HashMap<u64, bool>>,
        next: AtomicU64,
    }

    impl Mem {
        fn new() -> Self {
            let mut data = vec![0u8; 8 << 20];
            let mut x = 0x9E37_79B9_7F4A_7C15u64;
            for c in data.chunks_mut(8) {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                c.copy_from_slice(&x.to_le_bytes()[..c.len()]);
            }
            Mem {
                data,
                opens: Mutex::new(HashMap::new()),
                next: AtomicU64::new(1),
            }
        }
    }

    impl Provider for Mem {
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                access: Access::Read,
                immutable: true,
                slow: false,
                preferred_block: None,
                case: CaseMatch::Insensitive,
            }
        }
        fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
            Ok(match p.rel.to_ascii_lowercase().as_str() {
                FAST | SLOW => Some(Stat {
                    kind: KIND_FILE,
                    size: FILE_LEN,
                    mtime: 0,
                }),
                "" | "data" => Some(Stat {
                    kind: KIND_DIR,
                    size: 0,
                    mtime: 0,
                }),
                _ => None,
            })
        }
        fn readdir(&self, _p: VPath) -> Result<Vec<DirEntry>, i32> {
            Ok(Vec::new())
        }
        fn open(&self, p: VPath, _flags: u32) -> Result<(Handle, u64, bool), i32> {
            let slow = match p.rel.to_ascii_lowercase().as_str() {
                FAST => false,
                SLOW => true,
                _ => return Err(vfs_provider::not_found()),
            };
            let h = self.next.fetch_add(1, Ordering::Relaxed);
            self.opens.lock().unwrap().insert(h, slow);
            Ok((h, FILE_LEN, false))
        }
        fn close(&self, h: Handle) -> Result<(), i32> {
            self.opens
                .lock()
                .unwrap()
                .remove(&h)
                .map(|_| ())
                .ok_or_else(vfs_provider::bad_fh)
        }
        fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
            let slow = *self
                .opens
                .lock()
                .unwrap()
                .get(&h)
                .ok_or_else(vfs_provider::bad_fh)?;
            if slow {
                std::thread::sleep(SLOW_READ);
            }
            if offset >= FILE_LEN {
                return Ok(0);
            }
            let n = buf.len().min((FILE_LEN - offset) as usize);
            let o = (offset as usize) % (self.data.len() - n.max(1) + 1);
            buf[..n].copy_from_slice(&self.data[o..o + n]);
            Ok(n)
        }
    }

    /// The operations a game thread makes, under one client discipline.
    trait Client: Sync {
        fn getattr(&self, p: &str) -> bool;
        fn open(&self, p: &str) -> u64;
        fn read(&self, fh: u64, offset: u64, buf: &mut [u8]) -> usize;
    }

    /// The shim before the change: one lock around every round trip, and the
    /// chunking and pipelining `FuseClient::read_fragmented` had.
    struct Locked<'a> {
        c: RingClient<'a, SpinNotifier>,
        seg: &'a SharedSeg,
        lock: Mutex<()>,
    }

    impl Client for Locked<'_> {
        fn getattr(&self, p: &str) -> bool {
            let _g = self.lock.lock().unwrap();
            let r = self
                .c
                .submit(P::OP_GETATTR, 0, &P::encode_path_req(0, p))
                .unwrap();
            P::decode_getattr_resp(&r.payload).unwrap().found
        }
        fn open(&self, p: &str) -> u64 {
            let _g = self.lock.lock().unwrap();
            let r = self
                .c
                .submit(P::OP_OPEN, 0, &P::encode_open_req(0, P::OPEN_READ, p))
                .unwrap();
            assert_eq!(r.status, P::ST_OK);
            P::decode_open_resp(&r.payload).unwrap().fh
        }
        fn read(&self, fh: u64, offset: u64, buf: &mut [u8]) -> usize {
            let _g = self.lock.lock().unwrap();
            let bulk_chunk = 1 << 20;
            let inline_chunk = PAYLOAD_CAP as usize - 8;
            let pipeline = if buf.len() >= 4 << 20 { 8 } else { 4 };
            let mut filled = 0;
            while filled < buf.len() {
                let mut reqs = Vec::new();
                let mut wants = Vec::new();
                let mut off = filled;
                while reqs.len() < pipeline && off < buf.len() {
                    let rem = buf.len() - off;
                    let bulk = rem >= 64 * 1024;
                    let chunk = if bulk {
                        rem.min(bulk_chunk)
                    } else {
                        rem.min(inline_chunk)
                    };
                    reqs.push((
                        P::OP_READ,
                        if bulk { P::FLAG_READ_BULK } else { 0 },
                        P::encode_read_req(&P::ReadReq {
                            fh,
                            offset: offset + off as u64,
                            len: chunk as u32,
                        }),
                    ));
                    wants.push(chunk);
                    off += chunk;
                }
                let (resps, held) = self.c.submit_many_held(&reqs).unwrap();
                let mut got = 0;
                for (r, w) in resps.iter().zip(&wants) {
                    assert_eq!(r.status, P::ST_OK);
                    let dest = &mut buf[filled + got..filled + got + *w];
                    let n = if P::is_read_resp_bulk(&r.payload) {
                        let (bn, aoff) = P::decode_read_bulk_resp(&r.payload).unwrap();
                        let n = (bn as usize).min(dest.len());
                        self.seg.copy_to(aoff as usize, &mut dest[..n]).unwrap();
                        n
                    } else {
                        P::decode_read_resp_into(&r.payload, dest).unwrap()
                    };
                    got += n;
                }
                self.c.release_slots(&held);
                filled += got;
                if got == 0 {
                    break;
                }
            }
            filled
        }
    }

    /// The shim's client notifier in file-backed mode, in native calls: spin
    /// for the response, and once it is late yield, then sleep a millisecond
    /// at a time (`WakeServerSpinClient` in `vfs-shim`).
    struct ShimNotifier;

    impl Notifier for ShimNotifier {
        fn wait_client(&self, _slot: u32) {
            core::hint::spin_loop();
        }
        fn idle_client(&self, _slot: u32, waited: Duration) {
            if waited < Duration::from_millis(20) {
                std::thread::yield_now();
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// The shim now: no lock, reads counted against a gate sized from the
    /// worker count the director published.
    struct Gated<'a> {
        c: RingClient<'a, ShimNotifier>,
        gate: DataGate,
        plan: ReadPlan,
    }

    impl<'a> Gated<'a> {
        fn new(seg: &'a SharedSeg) -> Self {
            let c = RingClient::new(seg, ShimNotifier).unwrap();
            let gate = DataGate::for_ring(seg, &c.geom());
            Gated {
                c,
                gate,
                // `FuseClient::read_fragmented`'s plan for this geometry.
                plan: ReadPlan {
                    bulk_threshold: 64 * 1024,
                    bulk_chunk: 1 << 20,
                    inline_chunk: PAYLOAD_CAP as usize - 8,
                    depth: 4,
                    depth_stream: 8,
                    stream_bytes: 4 << 20,
                },
            }
        }
    }

    impl Client for Gated<'_> {
        fn getattr(&self, p: &str) -> bool {
            let r = self
                .c
                .submit(P::OP_GETATTR, 0, &P::encode_path_req(0, p))
                .unwrap();
            P::decode_getattr_resp(&r.payload).unwrap().found
        }
        fn open(&self, p: &str) -> u64 {
            let r = self
                .c
                .submit(P::OP_OPEN, 0, &P::encode_open_req(0, P::OPEN_READ, p))
                .unwrap();
            assert_eq!(r.status, P::ST_OK);
            P::decode_open_resp(&r.payload).unwrap().fh
        }
        fn read(&self, fh: u64, offset: u64, buf: &mut [u8]) -> usize {
            vfs_ipc::read_fragmented(&self.c, &self.gate, &self.plan, fh, offset, buf).unwrap()
        }
    }

    fn pct(v: &[f64], q: f64) -> f64 {
        v[((v.len() as f64 * q) as usize).min(v.len() - 1)]
    }

    fn line(name: &str, mut v: Vec<f64>) {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        println!(
            "  {name:<34} n={:<7} p50={:>8.2} mean={:>8.2} p99={:>9.2} max={:>10.2}  (us)",
            v.len(),
            pct(&v, 0.5),
            mean,
            pct(&v, 0.99),
            v[v.len() - 1]
        );
    }

    fn time_n(n: usize, warm: usize, mut f: impl FnMut()) -> Vec<f64> {
        for _ in 0..warm {
            f();
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let t = Instant::now();
            f();
            out.push(t.elapsed().as_nanos() as f64 / 1000.0);
        }
        out
    }

    /// One thread, hot ring: the cost the change must not raise.
    fn single(c: &dyn Client) {
        println!(" single thread, hot ring");
        line(
            "GETATTR hit",
            time_n(50_000, 5_000, || assert!(c.getattr(FAST))),
        );
        let fh = c.open(FAST);
        for (name, len, n) in [
            ("READ 1 B", 1usize, 50_000usize),
            ("READ 4 KiB", 4096, 50_000),
            ("READ 64 KiB (arena)", 65_536, 10_000),
            ("READ 1 MiB (arena)", 1 << 20, 1_000),
            ("READ 4 MiB (pipelined)", 4 << 20, 200),
        ] {
            let mut buf = vec![0u8; len];
            let mut off = 0u64;
            let v = time_n(n, n / 10 + 2, || {
                assert_eq!(c.read(fh, off, &mut buf), len);
                off = (off + len as u64) % (FILE_LEN - len as u64);
            });
            line(name, v);
        }
    }

    /// `stat` threads of GETATTR for `secs` beside `readers` threads that each
    /// read 4 MiB of the slow file over and over: more blocked reads than the
    /// director has workers. What is measured is whether a request that is not
    /// a read still gets a worker.
    fn stats_beside_blocked_readers(c: &dyn Client, readers: usize, stat: usize, secs: f64) {
        let stop = AtomicBool::new(false);
        let sfh = c.open(SLOW);
        let mut all: Vec<f64> = Vec::new();
        std::thread::scope(|s| {
            for _ in 0..readers {
                s.spawn(|| {
                    let mut buf = vec![0u8; 4 << 20];
                    while !stop.load(Ordering::Relaxed) {
                        assert_eq!(c.read(sfh, 0, &mut buf), buf.len());
                    }
                });
            }
            std::thread::sleep(Duration::from_millis(20));
            let hs: Vec<_> = (0..stat)
                .map(|_| {
                    let stop = &stop;
                    s.spawn(move || {
                        let mut lat = Vec::with_capacity(1 << 20);
                        while !stop.load(Ordering::Relaxed) {
                            let t = Instant::now();
                            assert!(c.getattr(FAST));
                            lat.push(t.elapsed().as_nanos() as f64 / 1000.0);
                        }
                        lat
                    })
                })
                .collect();
            std::thread::sleep(Duration::from_secs_f64(secs));
            stop.store(true, Ordering::Relaxed);
            for h in hs {
                all.extend(h.join().unwrap());
            }
        });
        all.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let label = format!("{readers} blocked readers + {stat} stat threads");
        if all.is_empty() {
            println!("  {label:<34} no stat completed");
            return;
        }
        println!(
            "  {label:<34} {:>9.0} stats/s      p50={:>7.2} p99={:>8.2} p99.99={:>10.2} max={:>10.2} (us)  over 10 ms: {}",
            all.len() as f64 / secs,
            pct(&all, 0.5),
            pct(&all, 0.99),
            pct(&all, 0.9999),
            all[all.len() - 1],
            all.iter().filter(|&&us| us > 10_000.0).count()
        );
    }

    /// `fast` threads of 4 KiB reads for `secs`, beside an optional thread that
    /// reads 4 MiB of the slow file over and over (four 1 MiB requests in
    /// flight, each holding a worker for [`SLOW_READ`]), [`SLOW_GAP`] apart.
    fn concurrent(c: &dyn Client, fast: usize, slow: bool, secs: f64) {
        let stop = AtomicBool::new(false);
        let fh = c.open(FAST);
        let sfh = c.open(SLOW);
        let slow_done = AtomicU64::new(0);
        let mut all: Vec<f64> = Vec::new();
        let t0 = Instant::now();
        std::thread::scope(|s| {
            if slow {
                s.spawn(|| {
                    let mut buf = vec![0u8; 4 << 20];
                    while !stop.load(Ordering::Relaxed) {
                        assert_eq!(c.read(sfh, 0, &mut buf), buf.len());
                        slow_done.fetch_add(1, Ordering::Relaxed);
                        // A game thread does something with what it read.
                        // Without the pause this loop re-takes an unfair lock
                        // at once and the `locked` client starves outright,
                        // which overstates what the lock cost.
                        std::thread::sleep(SLOW_GAP);
                    }
                });
                // Let the slow read get in flight before the clock starts.
                std::thread::sleep(Duration::from_millis(20));
            }
            let hs: Vec<_> = (0..fast)
                .map(|t| {
                    let stop = &stop;
                    s.spawn(move || {
                        let mut buf = vec![0u8; 4096];
                        let mut off = (t as u64) << 20;
                        let mut lat = Vec::with_capacity(1 << 20);
                        while !stop.load(Ordering::Relaxed) {
                            let t = Instant::now();
                            assert_eq!(c.read(fh, off, &mut buf), 4096);
                            lat.push(t.elapsed().as_nanos() as f64 / 1000.0);
                            off = (off + 4096) % (FILE_LEN - 4096);
                        }
                        lat
                    })
                })
                .collect();
            std::thread::sleep(Duration::from_secs_f64(secs));
            stop.store(true, Ordering::Relaxed);
            for h in hs {
                all.extend(h.join().unwrap());
            }
        });
        let el = t0.elapsed().as_secs_f64();
        all.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let label = if slow {
            format!("{} requesters ({fast} fast + 1 slow)", fast + 1)
        } else {
            format!("{fast} requesters (all fast)")
        };
        if all.is_empty() {
            println!("  {label:<34} no fast operation completed");
            return;
        }
        println!(
            "  {label:<34} {:>9.0} fast ops/s   p50={:>7.2} p99={:>8.2} p99.99={:>10.2} max={:>10.2} (us)  over 10 ms: {}{}",
            all.len() as f64 / secs,
            pct(&all, 0.5),
            pct(&all, 0.99),
            pct(&all, 0.9999),
            all[all.len() - 1],
            all.iter().filter(|&&us| us > 10_000.0).count(),
            if slow {
                format!(
                    "   slow 4 MiB reads done: {} in {el:.1}s",
                    slow_done.load(Ordering::Relaxed)
                )
            } else {
                String::new()
            }
        );
    }

    fn suite(name: &str, c: &dyn Client, secs: f64) {
        println!("\n== client: {name} ==");
        single(c);
        println!(" concurrent 4 KiB reads, {secs} s each");
        for n in [1usize, 4, 8, 16] {
            concurrent(c, n, false, secs);
        }
        println!(
            " the same with one requester reading a file whose every 1 MiB read takes {} ms",
            SLOW_READ.as_millis()
        );
        for n in [1usize, 4, 8, 16] {
            if n > 1 {
                concurrent(c, n - 1, true, secs);
            }
        }
        println!(" stats while more reads are blocked than there are workers");
        stats_beside_blocked_readers(c, 8, 2, secs);
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().collect();
        let dir = Path::new(args.get(1).map(String::as_str).unwrap_or_else(|| {
            eprintln!("usage: ring-bench <scratch-dir> [locked|gated|both] [workers] [seconds]");
            std::process::exit(2);
        }));
        let which = args.get(2).map(String::as_str).unwrap_or("both");
        let workers: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(16);
        let secs: f64 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(3.0);
        std::fs::create_dir_all(dir).unwrap();

        let d = Arc::new(Director::new());
        d.mount(RootId::DEFAULT, Arc::new(Mem::new())).unwrap();
        let ring = dir.join("ring-bench.bin");
        let _ = std::fs::remove_file(&ring);
        let ipc =
            IpcServe::start_file_backed_with_workers(Arc::clone(&d), &ring, PAYLOAD_CAP, workers)
                .unwrap();
        println!(
            "ring file {} ({} MiB), {} workers",
            ring.display(),
            ipc.map_bytes >> 20,
            ipc.worker_count()
        );

        if which == "locked" || which == "both" {
            let c = Locked {
                c: ipc.client().unwrap(),
                seg: ipc.shared_seg(),
                lock: Mutex::new(()),
            };
            suite("locked (one process-wide lock)", &c, secs);
        }
        if which == "gated" || which == "both" {
            let c = Gated::new(ipc.shared_seg());
            println!("\ngate: {} data requests in flight at most", c.gate.limit());
            suite("gated (no lock, reads bounded below the workers)", &c, secs);
        }
        ipc.stop();
        let _ = std::fs::remove_file(&ring);
    }
}

#[cfg(unix)]
fn main() {
    imp::main();
}

// `vfs-unix` and the director's file-backed ring are unix-only; this keeps a
// Windows build of this crate green, like its `ring-file-server` sibling.
#[cfg(not(unix))]
fn main() {}
