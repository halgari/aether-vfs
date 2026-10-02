//! Several client threads on one ring: what bounds them, and the fragmented
//! read they all make.
//!
//! The ring is multi-producer by construction — a slot is claimed by
//! compare-and-swap, a response is awaited on that slot alone, and every slot
//! has its own arena bank — so a client needs no lock around a round trip.
//! What it does need is a bound on the requests that can be *slow*.
//!
//! A server thread runs one request start to finish, so a read whose provider
//! blocks (a block still coming from the network) holds its thread for as long
//! as that takes. If every server thread holds one, nothing is answered: not a
//! stat, not an open, not a read the RAM tier could serve at once. The old
//! process-wide lock hid this by never letting more than one call's pipeline
//! in flight. [`DataGate`] replaces it with the bound that is actually
//! wanted: reads and writes in flight stay below the server's worker count
//! ([`data_limit`]), which leaves a worker for everything else.
//!
//! Process-local on purpose. The gate lives in the client process and is made
//! of `std` primitives; nothing here crosses the ring, so nothing here needs a
//! primitive both a Wine process and a native director can reach.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use vfs_protocol::{
    decode_read_bulk_resp, decode_read_resp_into, encode_read_req, is_read_resp_bulk, ReadReq,
    FLAG_READ_BULK, OP_READ, ST_BAD_REQUEST, ST_IO_ERROR, ST_OK,
};

use crate::arena::DEFAULT_WORKER_COUNT;
use crate::endpoint::RingClient;
use crate::layout::ST_ABANDONED;
use crate::notifier::Notifier;
use crate::ring::{self, Geom, IpcError};
use crate::seg::SharedSeg;

/// How many data requests (reads, writes) may be in flight on a ring served
/// by `workers` threads with `slots` slots.
///
/// A quarter of the workers, and at least one, are kept back for requests
/// that are not data. `workers == 0` means the server did not say, and is
/// read as [`DEFAULT_WORKER_COUNT`]. A single worker cannot be shared out, so
/// its limit is 1 and a slow read there does hold everything up.
pub fn data_limit(workers: u32, slots: u32) -> u32 {
    let workers = if workers == 0 {
        DEFAULT_WORKER_COUNT as u32
    } else {
        workers
    }
    .min(slots.max(1));
    let reserve = (workers / 4).max(1);
    workers.saturating_sub(reserve).max(1)
}

/// How often a thread asleep in line looks again without being woken.
const POLL: Duration = Duration::from_millis(5);
/// A place in line not refreshed for this long belongs to a thread that is
/// gone (killed while it waited), and is passed over so the line can move.
const STALE: Duration = Duration::from_millis(500);
/// How long a caller in line watches for its permit before it sleeps for it.
/// Long against a small read's round trip, short against a blocked one.
const WATCH: Duration = Duration::from_micros(200);
/// Turns of a short spin: for a permit before getting in line, and for the
/// line's lock, which is held for a few instructions at a time.
const SPINS: u32 = 256;

/// A counting gate on the data requests one process has in flight.
///
/// What it protects is the director's workers, so a permit stands for **a
/// worker a data request may be holding**, not for a request somebody is
/// waiting on. Three rules follow:
///
/// - **A permit outlives a timeout.** A read whose client gave up is still
///   inside its worker; its slot is `ABANDONED`. The permit stays out
///   ([`Permits::retire`]) until the director has finished with the slot,
///   and is taken back then ([`DataGate::reclaim`]). Otherwise every
///   timed-out read would free a permit while holding a worker, and a
///   provider that had stopped answering would end up with every worker —
///   the stats and opens the gate exists to keep served included.
/// - **One call cannot take the gate.** A pipelined read takes at most half
///   the permits ([`DataGate::per_call`]), and takes permits beyond its first
///   only while more than [`DataGate::reserve`] stay free. So two deep reads
///   of slow content leave permits for a third thread's small read, however
///   long they run.
/// - **First come, first served.** A permit returned while callers are
///   waiting is handed to the one at the front of the line, not put back for
///   whoever asks next — so a pipeline that returns its permits and asks
///   again for its next batch goes behind a caller already waiting. Left to
///   a race, the thread that just released wins it every time against one
///   that has to be woken first.
///
/// Taking and returning permits is a compare-and-swap and an add when there
/// is a permit and nobody waiting — no system call, the cost the uncontended
/// lock it replaces had. Everything else is off the hot path.
///
/// Nothing here can wait for ever. A wait is bounded by the caller's
/// patience, the line's mutex is only ever tried, and a waiter that
/// disappears is passed over; so a thread killed at any point (a game
/// exiting kills all but one) costs the survivors at most a deadline, not a
/// hang.
pub struct DataGate {
    avail: AtomicU32,
    limit: u32,
    reserve: u32,
    per_call: u32,
    /// Callers in line that have not been served, readable without the lock.
    queued: AtomicU32,
    line: Mutex<VecDeque<Arc<Waiter>>>,
    /// Signalled when a sleeping waiter is handed its permit.
    moved: Condvar,
    /// What [`Waiter::seen_ms`] counts from.
    epoch: Instant,
    /// Slots whose permit is still out because the director still has the
    /// request. See [`Permits::retire`].
    retired: Mutex<Vec<u32>>,
    retired_count: AtomicU32,
}

const WAITING: u32 = 0;
const GRANTED: u32 = 1;
const GONE: u32 = 2;

/// One caller's place in line.
struct Waiter {
    /// `WAITING` until a release hands it a permit (`GRANTED`) or it leaves
    /// the line without one (`GONE`: it ran out of patience, or was passed
    /// over as stale). One compare-and-swap decides which.
    state: AtomicU32,
    /// Blocked on [`DataGate::moved`], or about to be, so a grant must
    /// signal it.
    asleep: AtomicBool,
    /// When its owner last looked, in ms since the gate's epoch.
    seen_ms: AtomicU64,
}

/// Permits held on a [`DataGate`]; returned when dropped.
pub struct Permits<'a> {
    gate: &'a DataGate,
    n: u32,
}

impl Permits<'_> {
    /// How many requests the holder may have in flight. At least 1.
    pub fn count(&self) -> usize {
        self.n.max(1) as usize
    }

    /// Leave one permit out for each of `slots`: requests this holder gave
    /// up on while the director was still processing them
    /// ([`ring::Abandon::Retired`]). Those permits are not returned when
    /// this is dropped; [`DataGate::reclaim`] takes each back once its slot
    /// is no longer `ABANDONED`.
    pub fn retire(&mut self, slots: &[u32]) {
        for &slot in slots {
            if self.n == 0 {
                return;
            }
            if self.gate.retire(slot) {
                self.n -= 1;
            }
        }
    }
}

impl Drop for Permits<'_> {
    fn drop(&mut self) {
        self.gate.release(self.n);
    }
}

/// `try_lock` that takes a poisoned mutex as usable: a panic elsewhere must
/// not turn every later read into an error.
fn try_lock<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match m.try_lock() {
        Ok(g) => Some(g),
        Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

/// `try_lock`, tried for a short spin. `None` if the lock could not be had:
/// its holder is busy for unusually long, or was killed holding it.
fn lock_briefly<T>(m: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    for _ in 0..SPINS {
        if let Some(g) = try_lock(m) {
            return Some(g);
        }
        core::hint::spin_loop();
    }
    None
}

/// Sleep for about `d` without `std::thread::sleep`, which on Windows builds
/// a waitable timer per call — handle traffic a shim inside its own NT hooks
/// should not make. A timed wait on a condition variable nobody signals is
/// in-process on every target.
fn nap(d: Duration) {
    let m = Mutex::new(());
    let cv = Condvar::new();
    let g = m.lock().unwrap_or_else(|e| e.into_inner());
    let _ = cv.wait_timeout(g, d);
}

impl DataGate {
    /// A gate admitting `limit` data requests at once (at least 1).
    pub fn new(limit: u32) -> Self {
        let limit = limit.max(1);
        DataGate {
            avail: AtomicU32::new(limit),
            limit,
            // A quarter held back for single requests, but never the whole
            // gate; and no call more than half, rounded up.
            reserve: (limit / 4).max(1).min(limit - 1),
            per_call: limit.div_ceil(2),
            queued: AtomicU32::new(0),
            line: Mutex::new(VecDeque::new()),
            moved: Condvar::new(),
            epoch: Instant::now(),
            retired: Mutex::new(Vec::new()),
            retired_count: AtomicU32::new(0),
        }
    }

    /// The gate for the ring in `seg`: [`data_limit`] of the worker count its
    /// server published and its slot count.
    pub fn for_ring(seg: &SharedSeg, geom: &Geom) -> Self {
        Self::new(data_limit(ring::worker_hint(seg), geom.slot_count))
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// A call takes permits beyond its first only while this many would
    /// still be free: what is kept for single requests.
    pub fn reserve(&self) -> u32 {
        self.reserve
    }

    /// The most permits one call can hold.
    pub fn per_call(&self) -> u32 {
        self.per_call
    }

    /// Permits currently out: held by callers, or kept for retired slots.
    pub fn in_flight(&self) -> u32 {
        self.limit - self.avail.load(Ordering::Relaxed).min(self.limit)
    }

    /// Callers waiting in line for a permit.
    pub fn waiting(&self) -> u32 {
        self.queued.load(Ordering::SeqCst)
    }

    /// Slots whose permit is being kept until the director is done with them.
    pub fn retired(&self) -> u32 {
        self.retired_count.load(Ordering::SeqCst)
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Take up to `want` permits without waiting: the first if there is one
    /// at all, each further one only while more than the reserve stays free.
    fn take(&self, want: u32) -> u32 {
        let mut cur = self.avail.load(Ordering::SeqCst);
        loop {
            if cur == 0 {
                return 0;
            }
            let extras = (cur - 1).saturating_sub(self.reserve).min(want - 1);
            let n = 1 + extras;
            match self
                .avail
                .compare_exchange_weak(cur, cur - n, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return n,
                Err(seen) => cur = seen,
            }
        }
    }

    /// Take between 1 and `want` permits (`want` capped at
    /// [`Self::per_call`]), waiting in line if there is none or if others
    /// are already waiting. `None` if `patience` ran out first.
    ///
    /// `ring` is where retired slots are looked up, to take back the permits
    /// of those the director has finished with.
    ///
    /// A caller never waits while holding a permit, so callers cannot
    /// deadlock each other however deep their pipelines are; one that wanted
    /// eight and got one simply runs a shallower pipeline this time.
    pub fn acquire(
        &self,
        ring: (&SharedSeg, &Geom),
        want: u32,
        patience: Duration,
    ) -> Option<Permits<'_>> {
        let want = want.clamp(1, self.per_call);
        if self.queued.load(Ordering::SeqCst) == 0 {
            let mut n = self.take(want);
            if n == 0 && self.reclaim(ring.0, ring.1) > 0 {
                n = self.take(want);
            }
            // Permits held by small reads come back within a round trip.
            let mut spins = 0;
            while n == 0 && spins < SPINS && self.queued.load(Ordering::SeqCst) == 0 {
                core::hint::spin_loop();
                n = self.take(want);
                spins += 1;
            }
            if n > 0 {
                return Some(Permits { gate: self, n });
            }
        }
        // Built only on success: a `Permits` made and dropped would return a
        // permit nobody took.
        self.wait_in_line(ring, patience)
            .then(|| Permits { gate: self, n: 1 })
    }

    /// Put a new place at the back of the line. `None` if the line's lock
    /// could not be had before `patience` ran out.
    fn join_line(&self, start: Instant, patience: Duration) -> Option<Arc<Waiter>> {
        let me = Arc::new(Waiter {
            state: AtomicU32::new(WAITING),
            asleep: AtomicBool::new(false),
            seen_ms: AtomicU64::new(self.now_ms()),
        });
        loop {
            if let Some(mut line) = lock_briefly(&self.line) {
                line.push_back(Arc::clone(&me));
                self.queued.fetch_add(1, Ordering::SeqCst);
                return Some(me);
            }
            if start.elapsed() >= patience {
                return None;
            }
            nap(Duration::from_millis(1));
        }
    }

    /// Wait at the back of the line for one permit. True when it was handed
    /// over; false when `patience` ran out first.
    #[cold]
    fn wait_in_line(&self, ring: (&SharedSeg, &Geom), patience: Duration) -> bool {
        let start = Instant::now();
        let Some(mut me) = self.join_line(start, patience) else {
            return false;
        };
        // A permit returned just before this place was visible went into
        // `avail`, where nobody will hand it on. Do that now.
        self.hand_out();
        loop {
            match me.state.load(Ordering::SeqCst) {
                GRANTED => return true,
                GONE => {
                    // Passed over as stale: this thread was off the processor
                    // for longer than a dead one is waited for. Start again
                    // at the back.
                    match self.join_line(start, patience) {
                        Some(again) => me = again,
                        None => return false,
                    }
                    self.hand_out();
                    continue;
                }
                _ => {}
            }
            let waited = start.elapsed();
            if waited >= patience {
                // Leave, unless a permit arrives at this very moment.
                if me
                    .state
                    .compare_exchange(WAITING, GONE, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    self.queued.fetch_sub(1, Ordering::SeqCst);
                    return false;
                }
                continue;
            }
            me.seen_ms.store(self.now_ms(), Ordering::Relaxed);
            if waited < WATCH {
                // A permit held by a small read is back within microseconds:
                // watch for it before paying for a sleep and a wake-up.
                for _ in 0..16 {
                    core::hint::spin_loop();
                }
                continue;
            }
            // Permits of retired slots come back only when somebody looks,
            // and a permit can be sitting in `avail` if its releaser could
            // not get the line's lock.
            self.reclaim(ring.0, ring.1);
            self.hand_out();
            // Announced before the last look, under the lock a grant is made
            // under: the grant either is seen here or sees a sleeper.
            me.asleep.store(true, Ordering::SeqCst);
            match lock_briefly(&self.line) {
                Some(line) => {
                    if me.state.load(Ordering::SeqCst) == WAITING {
                        let _ = self.moved.wait_timeout(line, POLL);
                    }
                }
                None => nap(Duration::from_millis(1)),
            }
        }
    }

    /// Hand free permits to the callers at the front of the line, in order.
    ///
    /// A place whose owner has left, or has not looked for [`STALE`], is
    /// dropped. If the line's lock cannot be had the permits stay in `avail`
    /// and the waiters, who call this themselves as they poll, pick them up.
    fn hand_out(&self) {
        if self.queued.load(Ordering::SeqCst) == 0 {
            return;
        }
        let Some(mut line) = lock_briefly(&self.line) else {
            return;
        };
        let now_ms = self.now_ms();
        let mut wake = false;
        while let Some(front) = line.front() {
            if front.state.load(Ordering::SeqCst) != WAITING {
                line.pop_front();
                continue;
            }
            let quiet_ms = now_ms.saturating_sub(front.seen_ms.load(Ordering::Relaxed));
            if quiet_ms >= STALE.as_millis() as u64 {
                if front
                    .state
                    .compare_exchange(WAITING, GONE, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    self.queued.fetch_sub(1, Ordering::SeqCst);
                }
                line.pop_front();
                continue;
            }
            // One permit out of `avail`, whatever the reserve: it is this
            // caller's first.
            if self
                .avail
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |a| a.checked_sub(1))
                .is_err()
            {
                break;
            }
            if front
                .state
                .compare_exchange(WAITING, GRANTED, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                self.queued.fetch_sub(1, Ordering::SeqCst);
                wake |= front.asleep.load(Ordering::SeqCst);
            } else {
                // It left at that moment; the permit goes to the next.
                self.avail.fetch_add(1, Ordering::SeqCst);
            }
            line.pop_front();
        }
        if wake {
            self.moved.notify_all();
        }
    }

    fn release(&self, n: u32) {
        if n == 0 {
            return;
        }
        // Added before the line is looked at, and a caller joins the line
        // before it looks at `avail` again: one of the two sees the other.
        self.avail.fetch_add(n, Ordering::SeqCst);
        self.hand_out();
    }

    /// Keep one permit out for `slot`. False if that could not be recorded,
    /// in which case the caller returns the permit as usual.
    fn retire(&self, slot: u32) -> bool {
        for _ in 0..50 {
            let Some(mut retired) = lock_briefly(&self.retired) else {
                nap(Duration::from_millis(1));
                continue;
            };
            if retired.contains(&slot) {
                // The slot was claimed again since it was last retired, so
                // the director finished with that earlier request: its permit
                // comes back now, and the one kept is this request's.
                drop(retired);
                self.release(1);
            } else {
                retired.push(slot);
                self.retired_count
                    .store(retired.len() as u32, Ordering::SeqCst);
            }
            return true;
        }
        false
    }

    /// Take back the permit of every retired slot the director has finished
    /// with (its state is no longer `ABANDONED`). Returns how many.
    ///
    /// Called when a caller finds no permit, so a gate drained by timed-out
    /// reads refills as the director works through them.
    pub fn reclaim(&self, seg: &SharedSeg, geom: &Geom) -> u32 {
        if self.retired_count.load(Ordering::SeqCst) == 0 {
            return 0;
        }
        let Some(mut retired) = try_lock(&self.retired) else {
            return 0;
        };
        let before = retired.len();
        retired.retain(|&slot| ring::slot_state(seg, geom, slot) == Some(ST_ABANDONED));
        let freed = (before - retired.len()) as u32;
        self.retired_count
            .store(retired.len() as u32, Ordering::SeqCst);
        drop(retired);
        self.release(freed);
        freed
    }
}

/// Submit one data request that is not a read — a write, a truncate, an open
/// that may copy a file up — under one permit of `gate`.
///
/// [`IpcError::Timeout`] also when no permit came within the client's
/// deadline: the request was never sent.
pub fn submit_data<N: Notifier>(
    c: &RingClient<'_, N>,
    gate: &DataGate,
    opcode: u32,
    flags: u32,
    payload: &[u8],
) -> Result<crate::endpoint::Response, IpcError> {
    let geom = c.geom();
    let mut permit = gate
        .acquire((c.seg(), &geom), 1, c.deadline())
        .ok_or(IpcError::Timeout)?;
    c.submit_reporting(opcode, flags, payload).map_err(|u| {
        permit.retire(&u.retired);
        u.error
    })
}

/// How [`read_fragmented`] cuts one read into ring requests.
#[derive(Debug, Clone, Copy)]
pub struct ReadPlan {
    /// A fragment at least this long travels through the bulk arena; a shorter
    /// one inline in the slot's payload. `usize::MAX` for a ring with no arena.
    pub bulk_threshold: usize,
    /// Most bytes one arena request carries (at most one bank).
    pub bulk_chunk: usize,
    /// Most bytes one inline request carries (the payload cap less its header).
    pub inline_chunk: usize,
    /// Requests in flight for an ordinary read.
    pub depth: usize,
    /// Requests in flight for a read of at least `stream_bytes`.
    pub depth_stream: usize,
    pub stream_bytes: usize,
}

/// Read `[offset, offset + buf.len())` of `fh` into `buf`, as pipelined ring
/// requests. Returns the bytes read, short only at end of file.
///
/// Large fragments use the **shared bulk arena** (the ring carries only length
/// and offset); small ones the slot's inline payload. Each batch takes its
/// permits from `gate` for as long as it is in flight and no longer, so one
/// long read does not keep its whole pipeline from other threads between
/// batches.
pub fn read_fragmented<N: Notifier>(
    c: &RingClient<'_, N>,
    gate: &DataGate,
    plan: &ReadPlan,
    fh: u64,
    offset: u64,
    buf: &mut [u8],
) -> Result<usize, i32> {
    if buf.is_empty() {
        return Ok(0);
    }
    let seg = c.seg();
    let slots = (c.geom().slot_count as usize).max(1);
    // Deep pipeline for multi-MiB sequential streams (section fill / BSA).
    let depth = if buf.len() >= plan.stream_bytes {
        plan.depth_stream
    } else {
        plan.depth
    }
    .clamp(1, slots);
    let mut filled = 0usize;

    while filled < buf.len() {
        let rem = buf.len() - filled;
        let per = if rem >= plan.bulk_threshold {
            plan.bulk_chunk
        } else {
            plan.inline_chunk
        }
        .max(1);
        // No permit within the client's deadline is the same failure as no
        // response within it, and nothing has been sent.
        let geom = c.geom();
        let mut permits = gate
            .acquire(
                (seg, &geom),
                rem.div_ceil(per).min(depth) as u32,
                c.deadline(),
            )
            .ok_or(ST_IO_ERROR)?;

        let mut reqs: Vec<(u32, u32, Vec<u8>)> = Vec::new();
        let mut wants: Vec<usize> = Vec::new();
        let mut batch_off = filled;
        while reqs.len() < permits.count() && batch_off < buf.len() {
            let rem = buf.len() - batch_off;
            let bulk = rem >= plan.bulk_threshold;
            let chunk = if bulk {
                rem.min(plan.bulk_chunk)
            } else {
                rem.min(plan.inline_chunk)
            };
            if chunk == 0 {
                break;
            }
            let flags = if bulk { FLAG_READ_BULK } else { 0 };
            reqs.push((
                OP_READ,
                flags,
                encode_read_req(&ReadReq {
                    fh,
                    offset: offset + batch_off as u64,
                    len: chunk as u32,
                }),
            ));
            wants.push(chunk);
            batch_off += chunk;
        }
        if reqs.is_empty() {
            break;
        }

        // Hold slots until bulk arena banks are copied — free-before-copy
        // races with bank reuse and can corrupt BSA streams (game then dies
        // with 0xC0000409 / bad archive parse after ~full Animations.bsa).
        let (responses, held) = c.submit_many_held_reporting(&reqs).map_err(|u| {
            // The director still has these: their permits stay out until it
            // is done with them.
            permits.retire(&u.retired);
            ST_IO_ERROR
        })?;

        let mut batch_filled = 0usize;
        let mut eof = false;
        let mut copy_err: Option<i32> = None;
        for (resp, want) in responses.iter().zip(wants.iter()) {
            if resp.status != ST_OK {
                copy_err = Some(resp.status);
                break;
            }
            let frag_start = filled + batch_filled;
            let dest = &mut buf[frag_start..frag_start + *want];
            let n = if is_read_resp_bulk(&resp.payload) {
                let (bn, aoff) = match decode_read_bulk_resp(&resp.payload) {
                    Some(x) => x,
                    None => {
                        copy_err = Some(ST_BAD_REQUEST);
                        break;
                    }
                };
                let n = (bn as usize).min(dest.len());
                // Shared arena → destination (one memcpy; not via ring).
                if n > 0 && seg.copy_to(aoff as usize, &mut dest[..n]).is_none() {
                    copy_err = Some(ST_IO_ERROR);
                    break;
                }
                n
            } else {
                match decode_read_resp_into(&resp.payload, dest) {
                    Some(n) => n,
                    None => {
                        copy_err = Some(ST_BAD_REQUEST);
                        break;
                    }
                }
            };
            batch_filled += n;
            if n < *want {
                eof = true;
                break;
            }
        }
        c.release_slots(&held);
        drop(permits);
        if let Some(st) = copy_err {
            return Err(st);
        }
        filled += batch_filled;
        if eof || batch_filled == 0 {
            break;
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::OP_READ as L_OP_READ;
    use crate::seg::OwnedSeg;

    const LONG: Duration = Duration::from_secs(30);

    /// A two-slot ring nobody serves: enough for a gate to look slots up in.
    fn ring2() -> (OwnedSeg, Geom) {
        let owned = OwnedSeg::new(4096);
        let geom = ring::init(owned.seg(), 2, 64).unwrap();
        (owned, geom)
    }

    #[test]
    fn the_limit_leaves_a_worker_for_requests_that_are_not_data() {
        // (workers, slots) -> data requests admitted.
        assert_eq!(data_limit(16, 32), 12);
        assert_eq!(data_limit(4, 32), 3);
        assert_eq!(data_limit(2, 32), 1);
        assert_eq!(data_limit(32, 32), 24);
        // Unknown worker count is read as the default (4).
        assert_eq!(data_limit(0, 32), 3);
        // More workers than slots could never all hold a request.
        assert_eq!(data_limit(64, 8), 6);
        // One worker cannot be shared out; the gate still admits a request.
        assert_eq!(data_limit(1, 32), 1);
    }

    #[test]
    fn the_reserve_and_the_share_of_one_call_follow_the_limit() {
        // limit -> (reserve, per call)
        for (limit, reserve, per_call) in [(12, 3, 6), (3, 1, 2), (24, 6, 12), (2, 1, 1), (1, 0, 1)]
        {
            let g = DataGate::new(limit);
            assert_eq!(
                (g.reserve(), g.per_call()),
                (reserve, per_call),
                "limit {limit}"
            );
        }
    }

    /// **Two deep reads cannot take the gate.** With 12 permits, a pipeline
    /// asking for eight gets six (half), a second gets three (it may not dip
    /// into the reserve beyond its first), and three single requests are
    /// still admitted without waiting. Before the reserve existed the two
    /// pipelines got 8 and 4 and the first single request parked.
    #[test]
    fn two_pipelines_leave_the_reserve_for_single_requests() {
        let (owned, geom) = ring2();
        let ring = (owned.seg(), &geom);
        let g = DataGate::new(12);
        let a = g.acquire(ring, 8, LONG).unwrap();
        assert_eq!(a.count(), 6, "one call holds at most half the gate");
        let b = g.acquire(ring, 8, LONG).unwrap();
        assert_eq!(b.count(), 3, "permits beyond the first stop at the reserve");
        assert_eq!(g.in_flight(), 9);
        // The reserve: three more callers, each admitted at once. A zero
        // patience makes "at once" the assertion rather than a timing.
        let singles: Vec<_> = (0..3)
            .map(|i| {
                g.acquire(ring, 1, Duration::ZERO)
                    .unwrap_or_else(|| panic!("single request {i} found no permit"))
            })
            .collect();
        assert_eq!(g.in_flight(), 12);
        // A further pipeline's first permit may come from anywhere, so with
        // the reserve partly free it still gets one — and only one.
        drop(singles);
        let c = g.acquire(ring, 8, Duration::ZERO).unwrap();
        assert_eq!(c.count(), 1);
        drop((a, b, c));
        assert_eq!(g.in_flight(), 0);
    }

    #[test]
    fn an_exhausted_gate_parks_the_caller_until_a_permit_returns() {
        let (owned, geom) = ring2();
        let g = DataGate::new(1);
        let held = g.acquire((owned.seg(), &geom), 1, LONG).unwrap();
        let got = AtomicBool::new(false);
        std::thread::scope(|s| {
            let t = s.spawn(|| {
                let p = g.acquire((owned.seg(), &geom), 3, LONG).unwrap();
                got.store(true, Ordering::SeqCst);
                p.count()
            });
            while g.waiting() == 0 {
                std::thread::yield_now();
            }
            assert!(
                !got.load(Ordering::SeqCst),
                "a second caller got through a gate of one while the permit was held"
            );
            drop(held);
            assert_eq!(t.join().unwrap(), 1);
        });
        assert_eq!(g.in_flight(), 0);
        assert_eq!(g.waiting(), 0);
    }

    /// **No barging.** A caller that releases its permits and asks again —
    /// what a pipelined read does between batches — goes behind a caller
    /// that is already waiting. Before the line existed the releasing thread
    /// took its permits straight back with a compare-and-swap, every time,
    /// while the waiter was still being woken.
    #[test]
    fn a_caller_that_just_released_waits_behind_one_already_in_line() {
        let (owned, geom) = ring2();
        let ring = (owned.seg(), &geom);
        let g = DataGate::new(2);
        let batch = g.acquire(ring, 2, LONG).unwrap();
        let first = g.acquire(ring, 1, LONG).unwrap();
        assert_eq!(g.in_flight(), 2);
        let waiter_served = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                let p = g.acquire(ring, 1, LONG).unwrap();
                waiter_served.store(true, Ordering::SeqCst);
                drop(p);
            });
            while g.waiting() == 0 {
                std::thread::yield_now();
            }
            // The batch ends and the next one is asked for at once.
            drop(batch);
            let again = g.acquire(ring, 2, LONG).unwrap();
            assert!(
                waiter_served.load(Ordering::SeqCst),
                "the releasing caller took its permit back ahead of a caller already waiting"
            );
            drop(again);
        });
        drop(first);
        assert_eq!(g.in_flight(), 0);
    }

    /// A wait at the gate ends: with `None` when patience runs out, and
    /// without leaving a place in line behind.
    #[test]
    fn a_wait_at_the_gate_is_bounded() {
        let (owned, geom) = ring2();
        let ring = (owned.seg(), &geom);
        let g = DataGate::new(1);
        let held = g.acquire(ring, 1, LONG).unwrap();
        assert!(g.acquire(ring, 1, Duration::from_millis(20)).is_none());
        assert_eq!(g.waiting(), 0, "a caller that gave up is still in line");
        drop(held);
        assert!(g.acquire(ring, 1, Duration::ZERO).is_some());
    }

    /// A thread killed while it waited leaves its place in line for ever.
    /// The line drops a place nobody has refreshed, so those behind it move.
    #[test]
    fn a_place_in_line_nobody_refreshes_is_dropped() {
        let (owned, geom) = ring2();
        let ring = (owned.seg(), &geom);
        let g = DataGate::new(1);
        {
            // A place whose owner last looked before the gate's own epoch
            // plus nothing: by the time anyone hands a permit out it is
            // older than `STALE`.
            let dead = Arc::new(Waiter {
                state: AtomicU32::new(WAITING),
                asleep: AtomicBool::new(false),
                seen_ms: AtomicU64::new(0),
            });
            g.line.lock().unwrap().push_back(Arc::clone(&dead));
            g.queued.store(1, Ordering::SeqCst);
            while g.now_ms() < STALE.as_millis() as u64 {
                std::thread::yield_now();
            }
        }
        // Not first in line, yet served: the dead place ahead was dropped.
        assert!(g.acquire(ring, 1, LONG).is_some());
        assert_eq!(g.waiting(), 0);
    }

    /// **A permit outlives a timeout.** The permit of a request the director
    /// still has stays out when its holder is dropped, and comes back only
    /// when the slot is no longer `ABANDONED`.
    #[test]
    fn a_retired_slots_permit_stays_out_until_the_slot_is_drained() {
        let (owned, geom) = ring2();
        let seg = owned.seg();
        let g = DataGate::new(2);

        // A request the server took and the client then gave up on.
        let slot = ring::claim_free(seg, &geom).unwrap();
        let id = ring::publish_request(seg, &geom, slot, L_OP_READ, 0, b"").unwrap();
        ring::server_take(seg, &geom).unwrap();
        let mut p = g.acquire((seg, &geom), 1, LONG).unwrap();
        assert_eq!(ring::abandon(seg, &geom, slot), Ok(ring::Abandon::Retired));
        p.retire(&[slot]);
        drop(p);
        assert_eq!(
            g.in_flight(),
            1,
            "the worker is still held, so the permit is too"
        );
        assert_eq!(g.retired(), 1);

        // Still abandoned: nothing to take back, and only one permit left.
        assert_eq!(g.reclaim(seg, &geom), 0);
        let other = g.acquire((seg, &geom), 1, Duration::ZERO).unwrap();
        assert!(g
            .acquire((seg, &geom), 1, Duration::from_millis(10))
            .is_none());

        // The director finishes: the next caller that finds no permit takes
        // the retired one back.
        ring::server_complete(seg, &geom, slot, id, 0, b"late").unwrap();
        let back = g.acquire((seg, &geom), 1, Duration::ZERO);
        assert!(
            back.is_some(),
            "the drained slot's permit was not taken back"
        );
        assert_eq!(g.retired(), 0);
        drop((other, back));
        assert_eq!(g.in_flight(), 0);
    }

    /// A slot retired twice before anyone reclaimed: it was claimed again in
    /// between, so the first request is done and exactly one permit is out.
    #[test]
    fn a_slot_retired_again_keeps_one_permit_not_two() {
        let (owned, geom) = ring2();
        let seg = owned.seg();
        let g = DataGate::new(3);
        for _ in 0..2 {
            let slot = ring::claim_free(seg, &geom).unwrap();
            assert_eq!(slot, 0);
            let id = ring::publish_request(seg, &geom, slot, L_OP_READ, 0, b"").unwrap();
            ring::server_take(seg, &geom).unwrap();
            let mut p = g.acquire((seg, &geom), 1, LONG).unwrap();
            ring::abandon(seg, &geom, slot).unwrap();
            p.retire(&[slot]);
            drop(p);
            assert_eq!(g.in_flight(), 1);
            assert_eq!(g.retired(), 1);
            // Drained, but no caller has looked yet.
            ring::server_complete(seg, &geom, slot, id, 0, b"").unwrap();
        }
        assert_eq!(g.reclaim(seg, &geom), 1);
        assert_eq!(g.in_flight(), 0);
    }

    #[test]
    fn many_threads_never_exceed_the_limit_and_all_get_through() {
        const LIMIT: u32 = 3;
        let (owned, geom) = ring2();
        let g = DataGate::new(LIMIT);
        let inside = AtomicU32::new(0);
        let peak = AtomicU32::new(0);
        let done = Arc::new(AtomicU32::new(0));
        std::thread::scope(|s| {
            for _ in 0..12 {
                s.spawn(|| {
                    for i in 0..2_000u32 {
                        let p = g.acquire((owned.seg(), &geom), 1 + i % 3, LONG).unwrap();
                        let now = inside.fetch_add(p.n, Ordering::SeqCst) + p.n;
                        peak.fetch_max(now, Ordering::SeqCst);
                        inside.fetch_sub(p.n, Ordering::SeqCst);
                        done.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= LIMIT);
        assert_eq!(done.load(Ordering::Relaxed), 24_000);
        assert_eq!(g.in_flight(), 0);
        assert_eq!(g.waiting(), 0);
    }
}
