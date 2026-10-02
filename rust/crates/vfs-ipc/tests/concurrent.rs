//! Several client threads on one ring: they do not wait for each other, a
//! request that times out cannot hand its late reply to a later one, and the
//! data gate keeps a worker free for requests that are not data.
//!
//! Everything here runs the shipped endpoints (`RingClient`, `RingServer`,
//! `read_fragmented`, `DataGate`) over an in-process segment with real server
//! threads; only the handler is the test's own.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use vfs_ipc::layout::{ST_ABANDONED, ST_FREE};
use vfs_ipc::ring::{self, Geom, IpcError};
use vfs_ipc::{
    read_fragmented, DataArena, DataGate, Notifier, OwnedSeg, ReadPlan, RingClient, RingServer,
    SharedSeg, SpinNotifier, CLIENT_SPIN_BUDGET,
};
use vfs_protocol as P;

/// Echo the payload back at once.
const OP_ECHO: u32 = 900;
/// Sleep for the payload's `u64` milliseconds, then answer `b"slept"`.
const OP_SLEEP: u32 = 901;

/// Reads of this handle take [`SLOW_READ`] each; every other handle is instant.
const FH_SLOW: u64 = 99;
const SLOW_READ: Duration = Duration::from_millis(300);

const PAYLOAD_CAP: u32 = 4096;
const BANK: usize = 64 * 1024;

/// Byte `i` of file `fh`. Depends on both, with a prime period, so a read
/// answered with another handle's bytes or at another offset is caught.
fn byte(fh: u64, i: u64) -> u8 {
    ((i % 251) as u8) ^ (fh as u8).wrapping_mul(37)
}

struct Fixture {
    owned: OwnedSeg,
    geom: Geom,
    arena_off: usize,
    arena_len: usize,
    stop: AtomicBool,
    /// Slow reads inside the handler right now, and the most there ever were.
    slow_now: AtomicU32,
    slow_peak: AtomicU32,
}

impl Fixture {
    fn new(slots: u32) -> Self {
        let stride = (32 + PAYLOAD_CAP as usize + 7) & !7;
        let arena_off = 40 + slots as usize * stride;
        let arena_len = slots as usize * BANK;
        let owned = OwnedSeg::new(arena_off + arena_len);
        let geom = ring::init(owned.seg(), slots, PAYLOAD_CAP).unwrap();
        Fixture {
            owned,
            geom,
            arena_off,
            arena_len,
            stop: AtomicBool::new(false),
            slow_now: AtomicU32::new(0),
            slow_peak: AtomicU32::new(0),
        }
    }

    fn seg(&self) -> &SharedSeg {
        self.owned.seg()
    }

    fn client(&self) -> RingClient<'_, SpinNotifier> {
        RingClient::with_geom(self.seg(), self.geom, SpinNotifier)
    }

    fn plan(&self) -> ReadPlan {
        ReadPlan {
            bulk_threshold: 8 * 1024,
            bulk_chunk: BANK,
            inline_chunk: PAYLOAD_CAP as usize - 8,
            depth: 4,
            depth_stream: 8,
            stream_bytes: 4 * BANK,
        }
    }

    /// One server thread's loop: what `IpcServe`'s worker does, with this
    /// test's handler in place of the director.
    fn serve(&self) {
        let ring = RingServer::new(self.seg(), SpinNotifier).unwrap();
        let arena = DataArena::new(
            self.seg(),
            self.arena_off,
            self.arena_len,
            self.geom.slot_count as usize,
        );
        while !self.stop.load(Ordering::Relaxed) {
            ring.serve_one(|req| match req.opcode {
                OP_ECHO => (P::ST_OK, req.payload.clone()),
                OP_SLEEP => {
                    let ms = u64::from_le_bytes(req.payload[..8].try_into().unwrap());
                    thread::sleep(Duration::from_millis(ms));
                    (P::ST_OK, b"slept".to_vec())
                }
                P::OP_READ => {
                    let r = P::decode_read_req(&req.payload).unwrap();
                    if r.fh == FH_SLOW {
                        let now = self.slow_now.fetch_add(1, Ordering::SeqCst) + 1;
                        self.slow_peak.fetch_max(now, Ordering::SeqCst);
                        thread::sleep(SLOW_READ);
                        self.slow_now.fetch_sub(1, Ordering::SeqCst);
                    }
                    let len = r.len as usize;
                    if req.flags & P::FLAG_READ_BULK != 0 {
                        let (off, n) = arena
                            .fill_bank(req.slot, len, |buf| {
                                for (j, b) in buf.iter_mut().enumerate() {
                                    *b = byte(r.fh, r.offset + j as u64);
                                }
                                Ok(buf.len())
                            })
                            .unwrap();
                        (P::ST_OK, P::encode_read_resp_bulk(n as u32, off))
                    } else {
                        let data: Vec<u8> =
                            (0..len).map(|j| byte(r.fh, r.offset + j as u64)).collect();
                        (P::ST_OK, P::encode_read_resp(&data))
                    }
                }
                _ => (P::ST_NOT_SUPPORTED, Vec::new()),
            })
            .unwrap();
        }
    }

    /// Run `body` with `workers` server threads serving the ring.
    fn with_workers<R>(&self, workers: usize, body: impl FnOnce() -> R) -> R {
        thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| self.serve());
            }
            let out = body();
            self.stop.store(true, Ordering::Relaxed);
            out
        })
    }

    fn state(&self, slot: u32) -> u32 {
        ring::slot_state(self.seg(), &self.geom, slot).unwrap()
    }

    /// Wait (bounded) until `slot` is in `want`.
    fn await_state(&self, slot: u32, want: u32) {
        let t = Instant::now();
        while self.state(slot) != want {
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "slot {slot} never reached state {want}; it is {}",
                self.state(slot)
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
}

fn assert_bytes(fh: u64, offset: u64, got: &[u8]) {
    if let Some(i) = (0..got.len()).find(|&i| got[i] != byte(fh, offset + i as u64)) {
        panic!(
            "fh {fh} offset {offset}: byte {i} of {} is {}, want {}",
            got.len(),
            got[i],
            byte(fh, offset + i as u64)
        );
    }
}

/// Eight threads, no lock between them, each reading its own handle in sizes
/// that travel inline, through one arena bank, and as a pipeline of banks.
/// Every read must come back whole and with its own handle's bytes: the slots
/// and the banks are per request, so nothing may leak across threads.
#[test]
fn concurrent_reads_from_many_threads_each_get_their_own_bytes() {
    let fx = Fixture::new(8);
    let gate = DataGate::new(vfs_ipc::data_limit(4, 8));
    let plan = fx.plan();
    fx.with_workers(4, || {
        thread::scope(|s| {
            for t in 0..8u64 {
                let (fx, gate, plan) = (&fx, &gate, &plan);
                s.spawn(move || {
                    let c = fx.client();
                    let fh = t + 1;
                    let sizes = [1usize, 700, 5_000, BANK, 3 * BANK + 123, 5 * BANK];
                    for i in 0..60usize {
                        let len = sizes[(i + t as usize) % sizes.len()];
                        let offset = (i as u64) * 1_000_003 + t;
                        let mut buf = vec![0u8; len];
                        let n = read_fragmented(&c, gate, plan, fh, offset, &mut buf).unwrap();
                        assert_eq!(n, len);
                        assert_bytes(fh, offset, &buf);
                    }
                });
            }
        });
    });
    assert_eq!(gate.in_flight(), 0, "every permit must come back");
    for slot in 0..8 {
        assert_eq!(fx.state(slot), ST_FREE, "slot {slot} was left held");
    }
}

/// One thread's request takes 400 ms. Another thread's requests, made while
/// it is in flight, are answered as if it were not there.
#[test]
fn a_slow_request_does_not_hold_up_a_fast_one() {
    let fx = Fixture::new(8);
    fx.with_workers(4, || {
        let slow_done = AtomicBool::new(false);
        thread::scope(|s| {
            s.spawn(|| {
                let r = fx
                    .client()
                    .submit(OP_SLEEP, 0, &400u64.to_le_bytes())
                    .unwrap();
                assert_eq!(r.payload, b"slept");
                slow_done.store(true, Ordering::SeqCst);
            });
            thread::sleep(Duration::from_millis(50));
            let c = fx.client();
            let t = Instant::now();
            let mut worst = Duration::ZERO;
            for i in 0..500u32 {
                let payload = i.to_le_bytes();
                let one = Instant::now();
                let r = c.submit(OP_ECHO, 0, &payload).unwrap();
                worst = worst.max(one.elapsed());
                assert_eq!(r.payload, payload);
            }
            assert!(
                !slow_done.load(Ordering::SeqCst),
                "the slow request finished first: the fast ones were made to wait for it \
                 ({:?} for 500, worst {worst:?})",
                t.elapsed()
            );
            assert!(
                worst < Duration::from_millis(150),
                "a fast request took {worst:?} while a slow one was in flight"
            );
        });
    });
}

/// Six threads read a file whose every read blocks its server thread, on a
/// ring with four workers. Ungated, four of them would hold all four workers
/// and nothing else would be answered until one returned. The gate admits
/// three, so the fourth worker keeps answering.
#[test]
fn reads_held_at_the_gate_leave_a_worker_for_other_requests() {
    let fx = Fixture::new(8);
    let gate = DataGate::new(vfs_ipc::data_limit(4, 8));
    assert_eq!(gate.limit(), 3);
    let plan = fx.plan();
    fx.with_workers(4, || {
        thread::scope(|s| {
            for t in 0..6u64 {
                let (fx, gate, plan) = (&fx, &gate, &plan);
                s.spawn(move || {
                    let c = fx.client();
                    let mut buf = vec![0u8; 100];
                    let n = read_fragmented(&c, gate, plan, FH_SLOW, t, &mut buf).unwrap();
                    assert_eq!(n, 100);
                    assert_bytes(FH_SLOW, t, &buf);
                });
            }
            thread::sleep(Duration::from_millis(60));
            assert_eq!(
                gate.in_flight(),
                3,
                "three slow reads in flight, three parked"
            );

            let c = fx.client();
            let t = Instant::now();
            for i in 0..200u32 {
                let payload = i.to_le_bytes();
                assert_eq!(c.submit(OP_ECHO, 0, &payload).unwrap().payload, payload);
            }
            assert!(
                t.elapsed() < SLOW_READ - Duration::from_millis(100),
                "200 echoes took {:?}: they waited for a slow read",
                t.elapsed()
            );
        });
    });
    assert_eq!(
        fx.slow_peak.load(Ordering::SeqCst),
        3,
        "the server must never have held more slow reads than the gate admits"
    );
    assert_eq!(gate.in_flight(), 0);
}

/// **The late reply, end to end.** A bulk read outlives its client's
/// deadline. The client gets `Timeout`; the slot is retired, not freed; every
/// request made while the server is still writing that slot — and its arena
/// bank — goes to another slot and gets its own bytes; and once the server
/// finishes, the slot is free and serves a request correctly.
#[test]
fn a_timed_out_request_retires_its_slot_until_the_late_reply_is_drained() {
    let fx = Fixture::new(2);
    let gate = DataGate::new(8);
    let plan = fx.plan();
    fx.with_workers(2, || {
        let impatient = fx.client().with_deadline(Duration::from_millis(40));
        let mut buf = vec![0u8; BANK];
        let t = Instant::now();
        let r = read_fragmented(&impatient, &gate, &plan, FH_SLOW, 0, &mut buf);
        assert_eq!(r, Err(P::ST_IO_ERROR), "the read must fail, not hang");
        assert!(t.elapsed() < SLOW_READ, "it gave up at its deadline");
        assert_eq!(gate.in_flight(), 0, "a failed read returns its permit");

        // The server still has the request: the slot is out of use.
        let retired = (0..2).find(|&s| fx.state(s) == ST_ABANDONED);
        let retired = retired.expect("the timed-out slot must be ABANDONED, not FREE");

        // Reads made meanwhile — the same size, so they use an arena bank —
        // never land in the retired slot and never see the slow file's bytes.
        let c = fx.client();
        let mut during = 0;
        while fx.state(retired) == ST_ABANDONED {
            let fh = 7;
            let offset = during * 13;
            let n = read_fragmented(&c, &gate, &plan, fh, offset, &mut buf).unwrap();
            assert_eq!(n, BANK);
            assert_bytes(fh, offset, &buf);
            during += 1;
        }
        assert!(during > 0, "no read was made while the slot was retired");

        // The server finished and drained it: free, and usable.
        fx.await_state(retired, ST_FREE);
        let (resps, held) = c
            .submit_many_held(&[
                (OP_ECHO, 0, b"first".to_vec()),
                (OP_ECHO, 0, b"second".to_vec()),
            ])
            .unwrap();
        assert!(held.contains(&retired), "both slots are in use again");
        assert_eq!(resps[0].payload, b"first");
        assert_eq!(resps[1].payload, b"second");
        c.release_slots(&held);
    });
    assert_eq!(fx.state(0), ST_FREE);
    assert_eq!(fx.state(1), ST_FREE);
}

/// A request no server ever takes: the client times out and the slot is free
/// at once, because nothing can still write to it.
#[test]
fn a_request_nobody_serves_times_out_and_frees_its_slot() {
    let fx = Fixture::new(2);
    let c = fx.client().with_deadline(Duration::from_millis(30));
    for _ in 0..5 {
        assert_eq!(
            c.submit(OP_ECHO, 0, b"hello").err(),
            Some(IpcError::Timeout)
        );
    }
    assert_eq!(fx.state(0), ST_FREE);
    assert_eq!(fx.state(1), ST_FREE);
}

/// A pipelined batch in which one request outlives the deadline: the whole
/// batch fails, the answered slots are freed, and only the slot the server
/// still holds is retired.
#[test]
fn a_batch_that_times_out_gives_every_slot_back() {
    let fx = Fixture::new(4);
    fx.with_workers(3, || {
        let c = fx.client().with_deadline(Duration::from_millis(40));
        let r = c.submit_many_held(&[
            (OP_ECHO, 0, b"a".to_vec()),
            (OP_SLEEP, 0, 250u64.to_le_bytes().to_vec()),
            (OP_ECHO, 0, b"c".to_vec()),
        ]);
        assert_eq!(r.err(), Some(IpcError::Timeout));
        let states: Vec<u32> = (0..4).map(|s| fx.state(s)).collect();
        assert_eq!(
            states.iter().filter(|&&s| s == ST_ABANDONED).count(),
            1,
            "exactly the sleeping request's slot is retired: {states:?}"
        );
        assert_eq!(
            states.iter().filter(|&&s| s == ST_FREE).count(),
            3,
            "{states:?}"
        );
        let retired = states.iter().position(|&s| s == ST_ABANDONED).unwrap() as u32;
        fx.await_state(retired, ST_FREE);
    });
}

/// A client notifier that records what it is asked to do while waiting.
struct Watching {
    idle_calls: AtomicU32,
    /// The shortest `waited` an `idle_client` call was ever given, in µs.
    least_waited_us: AtomicU32,
}

impl Notifier for &Watching {
    fn wait_client(&self, _slot: u32) {
        core::hint::spin_loop();
    }
    fn idle_client(&self, _slot: u32, waited: Duration) {
        self.idle_calls.fetch_add(1, Ordering::Relaxed);
        self.least_waited_us
            .fetch_min(waited.as_micros() as u32, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(1));
    }
}

/// A response that is late is waited for through `idle_client` — where the
/// shim yields and sleeps — and only once the spin budget is spent. The reply
/// is still collected, so the sleeping costs latency at most, never the
/// answer.
#[test]
fn a_late_response_is_waited_for_off_the_processor() {
    let fx = Fixture::new(2);
    let w = Watching {
        idle_calls: AtomicU32::new(0),
        least_waited_us: AtomicU32::new(u32::MAX),
    };
    fx.with_workers(1, || {
        let c = RingClient::with_geom(fx.seg(), fx.geom, &w);
        let r = c.submit(OP_SLEEP, 0, &60u64.to_le_bytes()).unwrap();
        assert_eq!(r.payload, b"slept");
    });
    let calls = w.idle_calls.load(Ordering::Relaxed);
    assert!(
        (1..=70).contains(&calls),
        "a 60 ms wait in 1 ms sleeps is a few dozen idle calls, not {calls}"
    );
    assert!(
        w.least_waited_us.load(Ordering::Relaxed) as u128 >= CLIENT_SPIN_BUDGET.as_micros(),
        "idle_client was called before the spin budget was spent"
    );
}

/// Slots are reused: far more requests than slots, from more threads than
/// slots, with a server thread count below both. Nothing is lost, nothing is
/// answered with another request's bytes, and the ring ends empty.
#[test]
fn slots_are_reused_across_threads_without_crosstalk() {
    let fx = Fixture::new(4);
    fx.with_workers(2, || {
        thread::scope(|s| {
            for t in 0..8u32 {
                let fx = &fx;
                s.spawn(move || {
                    let c = fx.client();
                    for i in 0..3_000u32 {
                        let payload = [t.to_le_bytes(), i.to_le_bytes()].concat();
                        let r = c.submit(OP_ECHO, 0, &payload).unwrap();
                        assert_eq!(r.status, P::ST_OK);
                        assert_eq!(r.payload, payload, "thread {t} request {i}");
                    }
                });
            }
        });
    });
    for slot in 0..4 {
        assert_eq!(fx.state(slot), ST_FREE);
    }
}
