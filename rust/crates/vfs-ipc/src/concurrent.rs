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

use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use vfs_protocol::{
    decode_read_bulk_resp, decode_read_resp_into, encode_read_req, is_read_resp_bulk, ReadReq,
    FLAG_READ_BULK, OP_READ, ST_BAD_REQUEST, ST_IO_ERROR, ST_OK,
};

use crate::arena::DEFAULT_WORKER_COUNT;
use crate::endpoint::RingClient;
use crate::notifier::Notifier;
use crate::ring::{self, Geom};
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

/// Turns of [`DataGate::top_up`]: a few microseconds.
const TOP_UP_SPINS: u32 = 128;

/// A counting gate on the data requests one process has in flight.
///
/// Taking and returning permits is a compare-and-swap and an add when the
/// gate is not exhausted — no system call, the cost the uncontended lock it
/// replaces had. A thread that finds no permit parks until one comes back.
pub struct DataGate {
    avail: AtomicU32,
    limit: u32,
    /// Threads parked (or about to park) in [`DataGate::acquire`].
    sleepers: AtomicU32,
    lock: Mutex<()>,
    freed: Condvar,
}

/// Permits held on a [`DataGate`]; returned when dropped.
pub struct Permits<'a> {
    gate: &'a DataGate,
    n: u32,
}

impl Permits<'_> {
    /// How many requests the holder may have in flight. At least 1.
    pub fn count(&self) -> usize {
        self.n as usize
    }
}

impl Drop for Permits<'_> {
    fn drop(&mut self) {
        self.gate.release(self.n);
    }
}

impl DataGate {
    /// A gate admitting `limit` data requests at once (at least 1).
    pub fn new(limit: u32) -> Self {
        let limit = limit.max(1);
        DataGate {
            avail: AtomicU32::new(limit),
            limit,
            sleepers: AtomicU32::new(0),
            lock: Mutex::new(()),
            freed: Condvar::new(),
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

    /// Permits currently held.
    pub fn in_flight(&self) -> u32 {
        self.limit - self.avail.load(Ordering::Relaxed).min(self.limit)
    }

    /// Take up to `want` permits without waiting. Returns how many (0 if none).
    fn try_take(&self, want: u32) -> u32 {
        let mut cur = self.avail.load(Ordering::SeqCst);
        loop {
            let n = cur.min(want);
            if n == 0 {
                return 0;
            }
            match self
                .avail
                .compare_exchange_weak(cur, cur - n, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return n,
                Err(seen) => cur = seen,
            }
        }
    }

    /// Take between 1 and `want` permits: as many of `want` as are free now or
    /// within a few microseconds, waiting longer only if there is none at all.
    ///
    /// Never parks while holding a permit, so callers cannot deadlock each
    /// other however deep their pipelines are; a caller that wanted eight and
    /// got one simply runs a shallower pipeline this time.
    pub fn acquire(&self, want: u32) -> Permits<'_> {
        let want = want.max(1);
        let mut n = self.try_take(want);
        if n == 0 {
            n = self.wait_for(want);
        }
        if n < want {
            n += self.top_up(want - n);
        }
        Permits { gate: self, n }
    }

    /// A short, bounded look for `more` permits on top of some already held.
    ///
    /// Without it a pipelined read beside many threads of small reads runs one
    /// request deep: those threads hold each permit for a few microseconds,
    /// so at any instant few are free, yet several come back within the time
    /// this takes. Bounded and never parked, so it cannot deadlock, and when
    /// the other permits are held by reads that really are blocked it costs a
    /// bulk read a few microseconds and gives up.
    #[cold]
    fn top_up(&self, more: u32) -> u32 {
        let mut got = 0;
        for _ in 0..TOP_UP_SPINS {
            core::hint::spin_loop();
            got += self.try_take(more - got);
            if got == more {
                break;
            }
        }
        got
    }

    #[cold]
    fn wait_for(&self, want: u32) -> u32 {
        // A permit usually comes back within one small read's round trip.
        for _ in 0..256 {
            core::hint::spin_loop();
            let n = self.try_take(want);
            if n > 0 {
                return n;
            }
        }
        let mut guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        // Announced before the re-check, and `release` adds before it looks
        // for sleepers: whichever of the two runs second sees the other.
        self.sleepers.fetch_add(1, Ordering::SeqCst);
        let n = loop {
            let n = self.try_take(want);
            if n > 0 {
                break n;
            }
            // The timeout is a backstop, not the mechanism: a missed wake-up
            // costs this long once, not a hang.
            guard = self
                .freed
                .wait_timeout(guard, Duration::from_millis(5))
                .map(|(g, _)| g)
                .unwrap_or_else(|e| e.into_inner().0);
        };
        self.sleepers.fetch_sub(1, Ordering::SeqCst);
        n
    }

    fn release(&self, n: u32) {
        self.avail.fetch_add(n, Ordering::SeqCst);
        if self.sleepers.load(Ordering::SeqCst) > 0 {
            // Under the lock, so the wake-up cannot fall between a sleeper's
            // re-check and its wait.
            let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            self.freed.notify_all();
        }
    }
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
        let permits = gate.acquire(rem.div_ceil(per).min(depth) as u32);

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
        let (responses, held) = c.submit_many_held(&reqs).map_err(|_| ST_IO_ERROR)?;

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
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

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
    fn acquire_takes_what_is_free_and_never_more_than_asked() {
        let g = DataGate::new(6);
        let a = g.acquire(4);
        assert_eq!(a.count(), 4);
        assert_eq!(g.in_flight(), 4);
        // Only two left: a caller wanting eight gets two, without waiting.
        let b = g.acquire(8);
        assert_eq!(b.count(), 2);
        assert_eq!(g.in_flight(), 6);
        drop(a);
        assert_eq!(g.in_flight(), 2);
        let c = g.acquire(1);
        assert_eq!(c.count(), 1);
        drop((b, c));
        assert_eq!(g.in_flight(), 0);
    }

    #[test]
    fn an_exhausted_gate_parks_the_caller_until_a_permit_returns() {
        let g = Arc::new(DataGate::new(1));
        let held = g.acquire(1);
        let got = Arc::new(AtomicBool::new(false));
        let t = {
            let (g, got) = (Arc::clone(&g), Arc::clone(&got));
            std::thread::spawn(move || {
                let p = g.acquire(3);
                got.store(true, Ordering::SeqCst);
                p.count()
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !got.load(Ordering::SeqCst),
            "a second caller got through a gate of one while the permit was held"
        );
        drop(held);
        assert_eq!(t.join().unwrap(), 1);
        assert_eq!(g.in_flight(), 0);
    }

    #[test]
    fn many_threads_never_exceed_the_limit_and_all_get_through() {
        const LIMIT: u32 = 3;
        let g = DataGate::new(LIMIT);
        let inside = AtomicU32::new(0);
        let peak = AtomicU32::new(0);
        std::thread::scope(|s| {
            for _ in 0..12 {
                s.spawn(|| {
                    for i in 0..2_000u32 {
                        let p = g.acquire(1 + i % 3);
                        let now = inside.fetch_add(p.n, Ordering::SeqCst) + p.n;
                        peak.fetch_max(now, Ordering::SeqCst);
                        inside.fetch_sub(p.n, Ordering::SeqCst);
                    }
                });
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= LIMIT);
        assert_eq!(g.in_flight(), 0);
    }
}
