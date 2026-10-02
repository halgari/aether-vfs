//! RingClient / RingServer blocking endpoints.

use core::cell::Cell;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::notifier::Notifier;
use crate::ring::{self, Geom, IpcError};
use crate::seg::SharedSeg;

// Geom is re-exported via vfs_ipc::ring / vfs_ipc::Geom for A4 client caching.

pub struct Response {
    pub status: i32,
    pub payload: Vec<u8>,
}

/// Why a request got no response, and which of its slots the server still
/// has: retired ([`ring::Abandon::Retired`]), their workers still held. A
/// caller that counts workers — `concurrent::DataGate` — needs the second.
#[derive(Debug)]
pub struct Unanswered {
    pub error: IpcError,
    pub retired: Vec<u32>,
}

impl From<IpcError> for Unanswered {
    fn from(error: IpcError) -> Self {
        Unanswered {
            error,
            retired: Vec::new(),
        }
    }
}

pub struct Request {
    pub slot: u32,
    pub opcode: u32,
    pub flags: u32,
    pub req_id: u64,
    pub payload: Vec<u8>,
}

pub struct RingClient<'a, N: Notifier> {
    seg: &'a SharedSeg,
    geom: Geom,
    notifier: N,
    /// How long a published request may go unanswered:
    /// [`crate::RESPONSE_DEADLINE`] unless [`RingClient::with_deadline`] says
    /// otherwise.
    deadline: std::time::Duration,
}

pub struct RingServer<'a, N: Notifier> {
    seg: &'a SharedSeg,
    geom: Geom,
    notifier: N,
    /// Where this server thread's next scan for a submitted slot starts: one
    /// past the slot it last served. See [`ring::server_take_from`].
    next: AtomicU32,
}

/// How long a client spins for a response before it starts calling
/// [`Notifier::idle_client`] instead.
///
/// Measured round trips are a few microseconds, a 1 MiB bulk read about 50
/// and a pipelined 4 MiB one a few hundred, all of it ring and memcpy; so
/// nothing the ring alone can answer reaches this, and the hot path stays a
/// pure spin with no system call. What is still unanswered after it is
/// waiting on the provider, and a waiter that keeps a core busy for that
/// takes the core from whatever else the process wanted to run. One waiter
/// did so under the old process-wide lock; without it there can be one per
/// thread.
pub const CLIENT_SPIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(1);

std::thread_local! {
    /// The slot this thread claimed last. Its next claim looks there first.
    static SLOT_HINT: Cell<u32> = const { Cell::new(0) };
}

impl<'a, N: Notifier> RingClient<'a, N> {
    pub fn new(seg: &'a SharedSeg, notifier: N) -> Result<Self, IpcError> {
        let geom = ring::open(seg)?;
        Ok(Self::with_geom(seg, geom, notifier))
    }

    /// Cached ring geometry (avoids re-validating the header on every call).
    pub fn geom(&self) -> Geom {
        self.geom
    }

    /// The segment this client's ring — and its bulk arena — lives in.
    pub fn seg(&self) -> &'a SharedSeg {
        self.seg
    }

    /// Build a client with a pre-validated geometry (**A4** reuse).
    pub fn with_geom(seg: &'a SharedSeg, geom: Geom, notifier: N) -> Self {
        RingClient {
            seg,
            geom,
            notifier,
            deadline: crate::RESPONSE_DEADLINE,
        }
    }

    /// This client, giving up on an unanswered request after `deadline`
    /// instead of [`crate::RESPONSE_DEADLINE`].
    pub fn with_deadline(mut self, deadline: std::time::Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// How long this client waits for a slot or for a response.
    pub fn deadline(&self) -> std::time::Duration {
        self.deadline
    }

    /// Claim a slot, looking at `start` first, waiting while the ring is full
    /// — the way a response is waited for, and for no longer than one.
    ///
    /// This used to be fifty million bare passes over the slots: seven
    /// seconds of one core at full tilt, measured, and then `RingFull`. A
    /// full ring was unreachable while one lock let a single call's pipeline
    /// in at a time; it is reachable now (gated data slots, retired slots, a
    /// slot per thread doing metadata), and what fills it is slow requests,
    /// so the wait is spin, then [`Notifier::idle_client`], bounded by the
    /// deadline.
    fn claim_slot_from(&self, start: u32) -> Result<u32, IpcError> {
        let began = std::time::Instant::now();
        let mut tries: u32 = 0;
        let mut idle = false;
        loop {
            if let Some(s) = ring::claim_free_from(self.seg, &self.geom, start) {
                return Ok(s);
            }
            if idle {
                let waited = began.elapsed();
                if waited > self.deadline {
                    return Err(IpcError::RingFull);
                }
                self.notifier
                    .idle_client(start % self.geom.slot_count.max(1), waited);
                continue;
            }
            tries = tries.wrapping_add(1);
            if tries.is_multiple_of(256) {
                let waited = began.elapsed();
                if waited > self.deadline {
                    return Err(IpcError::RingFull);
                }
                idle = waited >= CLIENT_SPIN_BUDGET;
            }
            core::hint::spin_loop();
        }
    }

    /// Wait for the response to `req_id` in `slot`, until this client's
    /// deadline ([`crate::RESPONSE_DEADLINE`] by default) has passed since
    /// `start` — when the request, or the batch it belongs to, was published.
    ///
    /// A **time** bound, not a try count: `wait_client` is advisory and its
    /// cost varies enormously by implementation. The shim's
    /// spins (a try count would expire in well under a second), while
    /// `EventNotifier`'s sleeps 1 ms per call (where the same count would be
    /// thirteen hours). Only wall-clock means the same thing to both.
    ///
    /// `Instant::now()` is checked once every 4096 iterations so a spinning
    /// notifier does not pay for a clock read per turn. Past
    /// [`CLIENT_SPIN_BUDGET`] the wait is no longer on the hot path: the clock
    /// is read every turn and the notifier is told how long it has been
    /// ([`Notifier::idle_client`]) so it can stop burning a core.
    fn await_response(
        &self,
        slot: u32,
        req_id: u64,
        start: std::time::Instant,
    ) -> Result<(i32, Vec<u8>), IpcError> {
        let mut tries: u32 = 0;
        let mut idle = false;
        loop {
            if let Some(r) = ring::take_response(self.seg, &self.geom, slot, req_id)? {
                return Ok(r);
            }
            if idle {
                let waited = start.elapsed();
                if waited > self.deadline {
                    return Err(IpcError::Timeout);
                }
                self.notifier.idle_client(slot, waited);
                continue;
            }
            tries = tries.wrapping_add(1);
            if tries.is_multiple_of(4096) {
                let waited = start.elapsed();
                if waited > self.deadline {
                    return Err(IpcError::Timeout);
                }
                idle = waited >= CLIENT_SPIN_BUDGET;
            }
            self.notifier.wait_client(slot);
        }
    }

    /// Give up on every slot in `slots`, whatever state each is in. Returns
    /// the ones a server is still processing: retired, not freed.
    ///
    /// Never a plain free: a slot whose request a server is still processing
    /// is retired until that server finishes, so its late reply cannot be read
    /// by the next request to claim the slot. See [`ring::abandon`].
    fn abandon_slots(&self, slots: &[u32]) -> Vec<u32> {
        let mut retired = Vec::new();
        for &slot in slots {
            if ring::abandon(self.seg, &self.geom, slot) == Ok(ring::Abandon::Retired) {
                retired.push(slot);
            }
        }
        self.notifier.notify_slot_free();
        retired
    }

    /// Submit a request and block (via the notifier / spin) until the response.
    pub fn submit(&self, opcode: u32, flags: u32, payload: &[u8]) -> Result<Response, IpcError> {
        self.submit_reporting(opcode, flags, payload)
            .map_err(|u| u.error)
    }

    /// [`Self::submit`], whose error also says which slot (none or one) was
    /// left with the server.
    pub fn submit_reporting(
        &self,
        opcode: u32,
        flags: u32,
        payload: &[u8],
    ) -> Result<Response, Unanswered> {
        if payload.len() > self.geom.payload_cap as usize {
            return Err(IpcError::PayloadTooLarge.into());
        }
        let slot = self.claim_slot_from(SLOT_HINT.with(|h| h.get()))?;
        SLOT_HINT.with(|h| h.set(slot));
        let outcome = ring::publish_request(self.seg, &self.geom, slot, opcode, flags, payload)
            .and_then(|req_id| {
                self.notifier.notify_server();
                self.await_response(slot, req_id, std::time::Instant::now())
            });
        let (status, payload) = match outcome {
            Ok(r) => r,
            Err(e) => {
                // Give the slot up even on timeout, or a stalled director
                // costs the ring a slot permanently and the next request sees
                // RingFull — but never by freeing a slot the server still has.
                return Err(Unanswered {
                    error: e,
                    retired: self.abandon_slots(&[slot]),
                });
            }
        };
        ring::free_slot(self.seg, &self.geom, slot)?;
        self.notifier.notify_slot_free();
        Ok(Response { status, payload })
    }

    /// **A5:** publish several requests (each on its own slot), wait for all, free slots.
    /// Returns responses in the same order as `reqs`.
    ///
    /// Bulk READ responses only carry `(len, arena_offset)` — the payload lives in
    /// the shared arena bank for that slot. We **must not free slots until the
    /// caller has finished reading those banks** (see [`Self::submit_many_held`]).
    /// Freeing early lets a concurrent claim reuse the bank and corrupt bulk data.
    pub fn submit_many(
        &self,
        reqs: &[(u32, u32, Vec<u8>)],
    ) -> Result<Vec<Response>, IpcError> {
        let (out, slots) = self.submit_many_held(reqs)?;
        for &slot in &slots {
            let _ = ring::free_slot(self.seg, &self.geom, slot);
            self.notifier.notify_slot_free();
        }
        Ok(out)
    }

    /// Like [`Self::submit_many`], but leaves slots **held** so bulk arena banks
    /// stay stable until the caller frees them via [`Self::release_slots`].
    pub fn submit_many_held(
        &self,
        reqs: &[(u32, u32, Vec<u8>)],
    ) -> Result<(Vec<Response>, Vec<u32>), IpcError> {
        self.submit_many_held_reporting(reqs).map_err(|u| u.error)
    }

    /// [`Self::submit_many_held`], whose error also lists the slots left with
    /// the server (retired, their workers still held).
    ///
    /// The whole batch has one deadline, counted from when it was published.
    /// A clock per request, awaited in order, let eight requests that each
    /// answered just inside the deadline keep one call for eight deadlines.
    pub fn submit_many_held_reporting(
        &self,
        reqs: &[(u32, u32, Vec<u8>)],
    ) -> Result<(Vec<Response>, Vec<u32>), Unanswered> {
        if reqs.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        for (_, _, p) in reqs {
            if p.len() > self.geom.payload_cap as usize {
                return Err(IpcError::PayloadTooLarge.into());
            }
        }
        let mut slots = Vec::with_capacity(reqs.len());
        let mut ids = Vec::with_capacity(reqs.len());
        let first = SLOT_HINT.with(|h| h.get());
        let mut start = first;
        for (opcode, flags, payload) in reqs {
            let published = self.claim_slot_from(start).and_then(|slot| {
                slots.push(slot);
                start = slot.wrapping_add(1);
                ring::publish_request(self.seg, &self.geom, slot, *opcode, *flags, payload)
            });
            match published {
                Ok(id) => ids.push(id),
                Err(e) => {
                    // The requests already published are with the server.
                    return Err(Unanswered {
                        error: e,
                        retired: self.abandon_slots(&slots),
                    });
                }
            }
        }
        SLOT_HINT.with(|h| h.set(slots[0]));
        self.notifier.notify_server();
        let published = std::time::Instant::now();
        let mut out = Vec::with_capacity(reqs.len());
        for (&slot, &req_id) in slots.iter().zip(&ids) {
            let (status, payload) = match self.await_response(slot, req_id, published) {
                Ok(r) => r,
                Err(e) => {
                    // Every slot this call holds must go back, not just this one.
                    return Err(Unanswered {
                        error: e,
                        retired: self.abandon_slots(&slots),
                    });
                }
            };
            out.push(Response { status, payload });
        }
        Ok((out, slots))
    }

    /// Free slots previously returned by [`Self::submit_many_held`].
    pub fn release_slots(&self, slots: &[u32]) {
        for &slot in slots {
            let _ = ring::free_slot(self.seg, &self.geom, slot);
            self.notifier.notify_slot_free();
        }
    }
}

impl<'a, N: Notifier> RingServer<'a, N> {
    pub fn new(seg: &'a SharedSeg, notifier: N) -> Result<Self, IpcError> {
        let geom = ring::open(seg)?;
        Ok(RingServer {
            seg,
            geom,
            notifier,
            next: AtomicU32::new(0),
        })
    }

    /// Handle at most one submitted request. Returns Ok(true) if one was handled,
    /// Ok(false) if none was pending (after an advisory `wait_server`).
    pub fn serve_one(
        &self,
        handler: impl FnOnce(&Request) -> (i32, Vec<u8>),
    ) -> Result<bool, IpcError> {
        let start = self.next.load(Ordering::Relaxed);
        let slot = match ring::server_take_from(self.seg, &self.geom, start) {
            Some(s) => {
                self.next.store(s.wrapping_add(1), Ordering::Relaxed);
                s
            }
            None => {
                self.notifier.wait_server();
                return Ok(false);
            }
        };
        let (opcode, flags, req_id, payload) =
            ring::read_request(self.seg, &self.geom, slot).ok_or(IpcError::BadResponse)?;
        let req = Request { slot, opcode, flags, req_id, payload };
        let (status, resp) = handler(&req);
        // Delivered, or dropped because the client had stopped waiting: either
        // way this request is done and the slot is no longer this thread's.
        ring::server_complete(self.seg, &self.geom, slot, req.req_id, status, &resp)?;
        self.notifier.notify_client(slot);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::OP_GETATTR;
    use crate::notifier::SpinNotifier;
    use crate::ring::{self, init};
    use crate::seg::OwnedSeg;

    #[test]
    fn serve_one_handles_a_prepublished_request() {
        // Single-threaded: pre-publish a request via primitives, then serve_one
        // finds it immediately (no blocking), then read the completed response.
        let owned = OwnedSeg::new(4096);
        let geom = init(owned.seg(), 2, 128).unwrap();
        let seg = owned.seg();

        let slot = ring::claim_free(seg, &geom).unwrap();
        let req_id = ring::publish_request(seg, &geom, slot, OP_GETATTR, 3, b"ping").unwrap();

        let server = RingServer::new(seg, SpinNotifier).unwrap();
        let handled = server
            .serve_one(|req| {
                assert_eq!(req.opcode, OP_GETATTR);
                assert_eq!(req.flags, 3);
                assert_eq!(req.payload, b"ping");
                (99, b"pong".to_vec())
            })
            .unwrap();
        assert!(handled);

        let (status, resp) = ring::take_response(seg, &geom, slot, req_id)
            .unwrap()
            .unwrap();
        assert_eq!(status, 99);
        assert_eq!(resp, b"pong");
    }

    #[test]
    fn serve_one_returns_false_when_idle() {
        let owned = OwnedSeg::new(4096);
        init(owned.seg(), 2, 128).unwrap();
        let server = RingServer::new(owned.seg(), SpinNotifier).unwrap();
        assert!(!server.serve_one(|_| (0, Vec::new())).unwrap());
    }
}
