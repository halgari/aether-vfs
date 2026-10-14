//! Several client threads on one ring: they do not wait for each other, a
//! request that times out cannot hand its late reply to a later one, and the
//! data gate keeps a worker free for requests that are not data.
//!
//! Everything here runs the shipped endpoints (`RingClient`, `RingServer`,
//! `read_fragmented`, `DataGate`) over an in-process segment with real server
//! threads; only the handler is the test's own.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
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

/// Block until the test opens the latch ([`Fixture::open_latch`]), then
/// answer `b"released"`.
const OP_HOLD: u32 = 902;
/// Block until the test's step counter reaches the payload's `u32`
/// ([`Fixture::step`]), then answer `b"stepped"`.
const OP_STEP: u32 = 903;

/// Reads of this handle block until the test opens the latch: a provider
/// that has stopped answering, for exactly as long as the test says.
const FH_HELD: u64 = 98;
/// How long a test waits for something that must happen before it calls the
/// run a failure. Never part of a passing run's logic.
const GIVE_UP: Duration = Duration::from_secs(20);

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
    /// Closed until the test opens it; held requests wait on it.
    latch: Mutex<bool>,
    latch_opened: Condvar,
    /// How many steps the test has let go; `OP_STEP(k)` waits for `k`.
    steps: Mutex<u32>,
    stepped: Condvar,
    /// Requests inside the handler waiting on the latch, and the most ever.
    held_now: AtomicU32,
    held_peak: AtomicU32,
}

impl Fixture {
    fn new(slots: u32) -> Self {
        let stride = (32 + PAYLOAD_CAP as usize + 7) & !7;
        let arena_off = vfs_ipc::layout::RING_HEADER_SIZE + slots as usize * stride;
        let arena_len = slots as usize * BANK;
        let owned = OwnedSeg::new(arena_off + arena_len);
        let geom = ring::init(owned.seg(), slots, PAYLOAD_CAP).unwrap();
        Fixture {
            owned,
            geom,
            arena_off,
            arena_len,
            stop: AtomicBool::new(false),
            latch: Mutex::new(false),
            latch_opened: Condvar::new(),
            steps: Mutex::new(0),
            stepped: Condvar::new(),
            held_now: AtomicU32::new(0),
            held_peak: AtomicU32::new(0),
        }
    }

    /// Server side: wait until the test opens the latch.
    fn hold(&self) {
        let now = self.held_now.fetch_add(1, Ordering::SeqCst) + 1;
        self.held_peak.fetch_max(now, Ordering::SeqCst);
        let mut open = self.latch.lock().unwrap();
        while !*open {
            open = self.latch_opened.wait(open).unwrap();
        }
        drop(open);
        self.held_now.fetch_sub(1, Ordering::SeqCst);
    }

    /// Server side: wait until the test has let step `k` go.
    fn await_step(&self, k: u32) {
        let mut n = self.steps.lock().unwrap();
        while *n < k {
            n = self.stepped.wait(n).unwrap();
        }
    }

    /// Let the next step go.
    fn step(&self) {
        *self.steps.lock().unwrap() += 1;
        self.stepped.notify_all();
    }

    /// Let every step, now and later, go.
    fn release_steps(&self) {
        *self.steps.lock().unwrap() = u32::MAX;
        self.stepped.notify_all();
    }

    /// Let every held request, now and later, go.
    fn open_latch(&self) {
        *self.latch.lock().unwrap() = true;
        self.latch_opened.notify_all();
    }

    /// Wait until `ready` holds. Panics, naming `what`, if it never does.
    fn until(&self, what: &str, ready: impl Fn() -> bool) {
        let t = Instant::now();
        while !ready() {
            assert!(t.elapsed() < GIVE_UP, "never happened: {what}");
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn held(&self) -> u32 {
        self.held_now.load(Ordering::SeqCst)
    }

    fn slots_in(&self, state: u32) -> usize {
        (0..self.geom.slot_count)
            .filter(|&s| self.state(s) == state)
            .count()
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
                OP_STEP => {
                    self.await_step(u32::from_le_bytes(req.payload[..4].try_into().unwrap()));
                    (P::ST_OK, b"stepped".to_vec())
                }
                OP_HOLD => {
                    self.hold();
                    (P::ST_OK, b"released".to_vec())
                }
                P::OP_READ => {
                    let r = P::decode_read_req(&req.payload).unwrap();
                    if r.fh == FH_HELD {
                        self.hold();
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
        /// Lets the server threads go when `body` ends — or panics: a failed
        /// assertion must fail the test, not leave the scope waiting on
        /// workers that are still holding requests.
        struct Stop<'a>(&'a Fixture);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.open_latch();
                self.0.release_steps();
                self.0.stop.store(true, Ordering::Relaxed);
            }
        }
        thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| self.serve());
            }
            let _stop = Stop(self);
            body()
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

/// A read of the held file with a short deadline, repeated until it times out
/// **with the director inside it**: its slot retired and its permit kept, so
/// that `gate.retired()` reaches `want_retired`. Returns once the server's
/// handler has the request (`fx.held() == want_retired`).
///
/// A short deadline races the server's pick-up. On a loaded machine the
/// client can give up before any worker has taken the request; the slot is
/// then still SUBMITTED, so the client frees it and returns the permit
/// (nothing can write to it any more), and the attempt proves nothing about
/// a read the director is inside. That outcome is checked for what it is and
/// retried, so the test proves the same property at any machine speed rather
/// than assuming a worker always wins a 40 ms race.
fn time_out_inside_the_director(
    fx: &Fixture,
    gate: &DataGate,
    plan: &ReadPlan,
    buf: &mut [u8],
    want_retired: u32,
) {
    let impatient = fx.client().with_deadline(Duration::from_millis(40));
    let t = Instant::now();
    loop {
        let r = read_fragmented(&impatient, gate, plan, FH_HELD, 0, buf);
        assert_eq!(r, Err(P::ST_IO_ERROR), "a held read must fail, not hang");
        if gate.retired() == want_retired {
            break;
        }
        // Timed out before a worker took it: nothing of it is left behind.
        assert_eq!(gate.retired(), want_retired - 1);
        assert_eq!(gate.in_flight(), want_retired - 1, "its permit came back");
        assert_eq!(fx.slots_in(ST_ABANDONED), want_retired as usize - 1);
        assert!(
            t.elapsed() < GIVE_UP,
            "no worker took a read within its deadline in {GIVE_UP:?}"
        );
    }
    // PROCESSING is set as a worker takes the slot, a moment before the
    // handler reaches the latch.
    fx.until("the timed-out read reached the handler", || {
        fx.held() == want_retired
    });
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

/// One thread's request is held by the server for as long as the test likes.
/// Another thread's requests, made meanwhile, are answered as if it were not
/// there: all five hundred return while the first is still held.
#[test]
fn a_slow_request_does_not_hold_up_a_fast_one() {
    let fx = Fixture::new(8);
    fx.with_workers(4, || {
        thread::scope(|s| {
            let slow = s.spawn(|| fx.client().submit(OP_HOLD, 0, b"").unwrap().payload);
            fx.until("the held request reached the server", || fx.held() == 1);
            let c = fx.client();
            for i in 0..500u32 {
                let payload = i.to_le_bytes();
                assert_eq!(c.submit(OP_ECHO, 0, &payload).unwrap().payload, payload);
            }
            assert_eq!(fx.held(), 1, "the held request is still held");
            assert!(!slow.is_finished());
            fx.open_latch();
            assert_eq!(slow.join().unwrap(), b"released");
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
                    let n = read_fragmented(&c, gate, plan, FH_HELD, t, &mut buf).unwrap();
                    assert_eq!(n, 100);
                    assert_bytes(FH_HELD, t, &buf);
                });
            }
            fx.until("three reads with the server, three in line", || {
                fx.held() == 3 && gate.waiting() == 3
            });
            assert_eq!(gate.in_flight(), 3);

            let c = fx.client();
            for i in 0..200u32 {
                let payload = i.to_le_bytes();
                assert_eq!(c.submit(OP_ECHO, 0, &payload).unwrap().payload, payload);
            }
            assert_eq!(fx.held(), 3, "the reads are all still blocked");
            fx.open_latch();
        });
    });
    assert_eq!(
        fx.held_peak.load(Ordering::SeqCst),
        3,
        "the server must never have held more reads than the gate admits"
    );
    assert_eq!(gate.in_flight(), 0);
    assert_eq!(gate.waiting(), 0);
}

/// **A read that timed out still holds its worker, and the gate knows.**
///
/// Four workers, so three data requests at once. A provider stops answering.
/// Three reads time out one after another; the director is still inside all
/// three. A fourth read must not be sent — it would take the last worker —
/// and a request that is not data must still be answered.
///
/// The gate used to give a timed-out read's permit straight back. Then the
/// fourth read was sent, every worker was held, and the echo below got no
/// answer for as long as the provider stayed stuck.
#[test]
fn reads_that_timed_out_keep_their_permits_until_the_director_is_done() {
    let fx = Fixture::new(8);
    let gate = DataGate::new(vfs_ipc::data_limit(4, 8));
    let plan = fx.plan();
    fx.with_workers(4, || {
        let impatient = fx.client().with_deadline(Duration::from_millis(40));
        let mut buf = vec![0u8; 100];
        for sent in 1..=3u32 {
            time_out_inside_the_director(&fx, &gate, &plan, &mut buf, sent);
            assert_eq!(fx.held(), sent, "the director is still inside read {sent}");
            assert_eq!(gate.in_flight(), sent, "so its permit is still out");
            assert_eq!(gate.retired(), sent);
        }
        assert_eq!(fx.slots_in(ST_ABANDONED), 3);

        // The fourth: refused at the gate, never sent.
        let r = read_fragmented(&impatient, &gate, &plan, FH_HELD, 0, &mut buf);
        assert_eq!(r, Err(P::ST_IO_ERROR));
        assert_eq!(
            fx.slots_in(ST_ABANDONED),
            3,
            "a fourth request reached the ring"
        );
        assert_eq!(fx.held(), 3);
        assert_eq!(gate.waiting(), 0, "it left the line when it gave up");

        // So the fourth worker is free, with the provider still stuck.
        let c = fx.client().with_deadline(GIVE_UP);
        assert_eq!(c.submit(OP_ECHO, 0, b"stat").unwrap().payload, b"stat");
        assert_eq!(fx.held(), 3);

        // The provider answers at last. The director drains the three slots,
        // and the next read that finds no permit takes theirs back.
        fx.open_latch();
        fx.until("the retired slots were drained", || {
            fx.slots_in(ST_ABANDONED) == 0
        });
        for i in 0..4u64 {
            let patient = fx.client().with_deadline(GIVE_UP);
            let n = read_fragmented(&patient, &gate, &plan, 7, i, &mut buf).unwrap();
            assert_eq!(n, 100);
            assert_bytes(7, i, &buf);
        }
        assert_eq!(
            gate.reclaim(fx.seg(), &fx.geom),
            0,
            "nothing left to take back"
        );
        assert_eq!(gate.retired(), 0);
        assert_eq!(gate.in_flight(), 0);
    });
    assert_eq!(fx.held_peak.load(Ordering::SeqCst), 3);
    assert_eq!(fx.slots_in(ST_FREE), 8);
}

/// **Two deep reads of content that is not coming do not take the gate.**
///
/// Sixteen workers, so twelve data requests at once. One thread reads eight
/// chunks of a blocked file and another four. Then a third thread reads one
/// byte of a file that is served at once.
///
/// The two pipelines used to be given 8 and 4 permits — all twelve — and the
/// third thread's read waited at the gate for as long as they ran. Now one
/// call holds at most half the gate and stops at the reserve, so they hold 6
/// and 3, and the small read goes straight through while both are still
/// blocked.
#[test]
fn two_deep_reads_of_blocked_content_leave_room_for_a_small_read() {
    let fx = Fixture::new(32);
    let gate = DataGate::new(vfs_ipc::data_limit(16, 32));
    assert_eq!((gate.limit(), gate.per_call(), gate.reserve()), (12, 6, 3));
    let plan = fx.plan();
    fx.with_workers(16, || {
        thread::scope(|s| {
            for chunks in [8usize, 4] {
                let (fx, gate, plan) = (&fx, &gate, &plan);
                s.spawn(move || {
                    let c = fx.client();
                    let mut buf = vec![0u8; chunks * BANK];
                    let n = read_fragmented(&c, gate, plan, FH_HELD, 0, &mut buf).unwrap();
                    assert_eq!(n, buf.len());
                    assert_bytes(FH_HELD, 0, &buf);
                });
                // One at a time, so which pipeline asked first is not left to
                // the scheduler: the eight-chunk read, then the four.
                let want = if chunks == 8 { 6 } else { 9 };
                fx.until("the pipeline's first batch reached the server", || {
                    fx.held() == want
                });
            }
            assert_eq!(
                gate.in_flight(),
                9,
                "6 for the first pipeline, 3 for the second"
            );
            assert_eq!(gate.waiting(), 0);

            // Both blocked. A zero-length wait at the gate would fail this
            // read, so that it succeeds is the assertion; the deadline only
            // bounds a failing run.
            let c = fx.client().with_deadline(GIVE_UP);
            let mut one = [0u8; 1];
            for i in 0..50u64 {
                let n = read_fragmented(&c, &gate, &plan, 7, i, &mut one).unwrap();
                assert_eq!((n, one[0]), (1, byte(7, i)));
                assert_eq!(fx.held(), 9, "both pipelines are still blocked");
            }
            fx.open_latch();
        });
    });
    assert!(
        fx.held_peak.load(Ordering::SeqCst) <= 9,
        "the pipelines held more than nine workers between them"
    );
    assert_eq!(gate.in_flight(), 0);
}

/// **The late reply, end to end.** A bulk read outlives its client's
/// deadline. The client gets `Timeout`; the slot is retired, not freed; every
/// request made while the server is still inside that slot — and its arena
/// bank — goes to another slot and gets its own bytes; and once the server
/// finishes, the slot is free and serves a request correctly.
///
/// The server holds the read on the latch rather than sleeping, so "while the
/// server is still inside it" is exactly as long as the test says, on any
/// machine.
#[test]
fn a_timed_out_request_retires_its_slot_until_the_late_reply_is_drained() {
    let fx = Fixture::new(2);
    let gate = DataGate::new(8);
    let plan = fx.plan();
    fx.with_workers(2, || {
        let mut buf = vec![0u8; BANK];
        time_out_inside_the_director(&fx, &gate, &plan, &mut buf, 1);
        assert_eq!(
            gate.in_flight(),
            1,
            "the director still has the request, so its permit stays out"
        );
        assert_eq!(gate.retired(), 1);

        // The server still has the request: the slot is out of use.
        let retired = (0..2).find(|&s| fx.state(s) == ST_ABANDONED);
        let retired = retired.expect("the timed-out slot must be ABANDONED, not FREE");

        // Reads made meanwhile — the same size, so they use an arena bank —
        // never land in the retired slot and never see the held file's bytes.
        let c = fx.client().with_deadline(GIVE_UP);
        for during in 0..50u64 {
            let fh = 7;
            let offset = during * 13;
            let n = read_fragmented(&c, &gate, &plan, fh, offset, &mut buf).unwrap();
            assert_eq!(n, BANK);
            assert_bytes(fh, offset, &buf);
            assert_eq!(
                fx.state(retired),
                ST_ABANDONED,
                "the slot stays retired while the server is inside it"
            );
        }
        assert_eq!(fx.held(), 1, "the late reply is still being held");

        // The server finishes and drains it: free, and usable.
        fx.open_latch();
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
        assert_eq!(gate.reclaim(fx.seg(), &fx.geom), 1, "its permit comes back");
        assert_eq!(gate.in_flight(), 0);
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
///
/// The long request is held on the latch, not slept, so it cannot finish
/// before the deadline. Whether a worker *took* it before the 40 ms deadline
/// is still up to the scheduler: if none did, the client frees its slot
/// (nothing can write to it), every slot comes back at once and the attempt
/// proves nothing about retiring, so it is checked for that and retried.
#[test]
fn a_batch_that_times_out_gives_every_slot_back() {
    let fx = Fixture::new(4);
    fx.with_workers(3, || {
        let c = fx.client().with_deadline(Duration::from_millis(40));
        let t = Instant::now();
        let retired = loop {
            let r = c.submit_many_held(&[
                (OP_ECHO, 0, b"a".to_vec()),
                (OP_HOLD, 0, Vec::new()),
                (OP_ECHO, 0, b"c".to_vec()),
            ]);
            assert_eq!(r.err(), Some(IpcError::Timeout));
            let states: Vec<u32> = (0..4).map(|s| fx.state(s)).collect();
            assert!(
                states.iter().all(|&s| s == ST_FREE || s == ST_ABANDONED),
                "a timed-out batch leaves no slot claimed or completed: {states:?}"
            );
            // Settle: an echo a worker was still inside when the deadline
            // fired drains by itself, and a held request reaches the latch a
            // moment after its worker took it.
            fx.until("the batch's slots settled", || {
                let free = fx.slots_in(ST_FREE);
                (fx.held() == 1 && fx.slots_in(ST_ABANDONED) == 1 && free == 3)
                    || (fx.held() == 0 && free == 4)
            });
            if fx.held() == 1 {
                break (0..4).find(|&s| fx.state(s) == ST_ABANDONED).unwrap();
            }
            assert!(
                t.elapsed() < GIVE_UP,
                "no worker took the held request within its deadline in {GIVE_UP:?}"
            );
        };
        // Exactly the held request's slot is retired, for as long as the
        // server is inside it.
        assert_eq!(fx.slots_in(ST_ABANDONED), 1);
        assert_eq!(fx.slots_in(ST_FREE), 3);
        let echo = fx.client().with_deadline(GIVE_UP);
        assert_eq!(echo.submit(OP_ECHO, 0, b"x").unwrap().payload, b"x");
        assert_eq!(fx.state(retired), ST_ABANDONED, "still retired while held");
        fx.open_latch();
        fx.await_state(retired, ST_FREE);
    });
}

/// A client notifier that records what it is asked to do while waiting, and
/// can open a fixture's latch on its `n`th idle call.
struct Watching<'a> {
    idle_calls: AtomicU32,
    /// The shortest `waited` an `idle_client` call was ever given, in µs.
    least_waited_us: AtomicU32,
    release_on_idle: Option<(u32, &'a Fixture)>,
}

impl Watching<'_> {
    fn new() -> Self {
        Watching {
            idle_calls: AtomicU32::new(0),
            least_waited_us: AtomicU32::new(u32::MAX),
            release_on_idle: None,
        }
    }
}

impl Notifier for &Watching<'_> {
    fn wait_client(&self, _slot: u32) {
        core::hint::spin_loop();
    }
    fn idle_client(&self, _slot: u32, waited: Duration) {
        let n = self.idle_calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.least_waited_us
            .fetch_min(waited.as_micros() as u32, Ordering::Relaxed);
        if let Some((at, fx)) = self.release_on_idle {
            if n == at {
                fx.open_latch();
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// A response that is late is waited for through `idle_client` — where the
/// shim yields and sleeps — and only once the spin budget is spent. The reply
/// is still collected, so the sleeping costs latency at most, never the
/// answer.
///
/// The request is held on the latch, and only the client's own idle wait
/// opens it (on its fifth `idle_client` call). So the reply provably came
/// back to a client that had left the spin for the idle path, without
/// assuming how long a 1 ms sleep takes — Windows rounds it up to its timer
/// tick, which made a call-count bound on a 60 ms sleep fail.
#[test]
fn a_late_response_is_waited_for_off_the_processor() {
    const RELEASE_AT: u32 = 5;
    let fx = Fixture::new(2);
    let w = Watching {
        release_on_idle: Some((RELEASE_AT, &fx)),
        ..Watching::new()
    };
    fx.with_workers(1, || {
        let c = RingClient::with_geom(fx.seg(), fx.geom, &w).with_deadline(GIVE_UP);
        let r = c.submit(OP_HOLD, 0, b"").unwrap();
        assert_eq!(r.payload, b"released");
    });
    let calls = w.idle_calls.load(Ordering::SeqCst);
    assert!(
        calls >= RELEASE_AT,
        "the reply came back before the idle wait released it: {calls} idle calls"
    );
    assert_eq!(fx.held_peak.load(Ordering::SeqCst), 1);
    assert!(
        w.least_waited_us.load(Ordering::Relaxed) as u128 >= CLIENT_SPIN_BUDGET.as_micros(),
        "idle_client was called before the spin budget was spent"
    );
}

/// A full ring is waited for the way a response is — spin, then idle — and
/// for no longer than the deadline. It used to be fifty million bare passes:
/// seconds of a core at full tilt.
#[test]
fn a_full_ring_is_waited_for_off_the_processor_and_not_for_ever() {
    let fx = Fixture::new(2);
    // Every slot claimed by requests that never complete; nobody serves.
    assert!(ring::claim_free(fx.seg(), &fx.geom).is_some());
    assert!(ring::claim_free(fx.seg(), &fx.geom).is_some());
    let w = Watching::new();
    let c = RingClient::with_geom(fx.seg(), fx.geom, &w).with_deadline(Duration::from_millis(30));
    assert_eq!(c.submit(OP_ECHO, 0, b"x").err(), Some(IpcError::RingFull));
    let batch = c.submit_many_held(&[(OP_ECHO, 0, b"x".to_vec())]);
    assert_eq!(batch.err(), Some(IpcError::RingFull));
    assert!(
        w.idle_calls.load(Ordering::Relaxed) > 0,
        "the wait for a slot never left the spin"
    );
    assert!(w.least_waited_us.load(Ordering::Relaxed) as u128 >= CLIENT_SPIN_BUDGET.as_micros());
}

/// One deadline for a whole batch. Three requests answered one after
/// another, each `STEP` after the one before, against a deadline shorter than
/// two steps: the second can only be answered past the deadline. With a clock
/// per request, started when its turn to be awaited came, each was "on time"
/// and the call ran past its deadline without noticing.
///
/// The steps are let go only by the client's own idle wait ([`Stepper`]),
/// which it calls only after checking that the deadline has not passed. So
/// with one clock for the batch the second step is not let go and the call
/// times out, short of a 100 ms stall between two adjacent statements. It
/// used to sleep on the server, and a client descheduled for the whole batch
/// woke to three finished answers and took them.
#[test]
fn a_batch_has_one_deadline_not_one_per_request() {
    // Step 2 comes 300 ms after the stepper's first idle call, which is after
    // publication; the client passes its 200 ms deadline check just before
    // each idle call. Letting step 2 go would need the client descheduled for
    // 100 ms between those two statements.
    const DEADLINE: Duration = Duration::from_millis(200);
    const STEP: Duration = Duration::from_millis(150);
    let fx = Fixture::new(4);
    let stepper = Stepper {
        fx: &fx,
        every: STEP,
        last: Mutex::new(None),
    };
    fx.with_workers(3, || {
        let c = RingClient::with_geom(fx.seg(), fx.geom, &stepper).with_deadline(DEADLINE);
        let r = c.submit_many_held(&[
            (OP_STEP, 0, 1u32.to_le_bytes().to_vec()),
            (OP_STEP, 0, 2u32.to_le_bytes().to_vec()),
            (OP_STEP, 0, 3u32.to_le_bytes().to_vec()),
        ]);
        assert_eq!(r.err(), Some(IpcError::Timeout));
        assert!(
            *fx.steps.lock().unwrap() < 2,
            "a step was let go after the batch's deadline had passed"
        );
        fx.release_steps();
        fx.until("every slot came back", || fx.slots_in(ST_FREE) == 4);
    });
}

/// A client notifier that lets the fixture's next step go each time `every`
/// has passed since the last one (counted from its first idle call, which
/// comes after the batch was published). It acts only inside `idle_client`,
/// and the client calls that only while its deadline has not passed.
struct Stepper<'a> {
    fx: &'a Fixture,
    every: Duration,
    last: Mutex<Option<Instant>>,
}

impl Notifier for &Stepper<'_> {
    fn wait_client(&self, _slot: u32) {
        core::hint::spin_loop();
    }
    fn idle_client(&self, _slot: u32, _waited: Duration) {
        let mut last = self.last.lock().unwrap();
        let now = Instant::now();
        match *last {
            None => *last = Some(now),
            Some(t) if now.duration_since(t) >= self.every => {
                self.fx.step();
                *last = Some(now);
            }
            Some(_) => {}
        }
        drop(last);
        thread::sleep(Duration::from_millis(1));
    }
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
