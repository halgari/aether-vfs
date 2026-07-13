//! RingClient / RingServer blocking endpoints.

use crate::notifier::Notifier;
use crate::ring::{self, Geom, IpcError};
use crate::seg::SharedSeg;

pub struct Response {
    pub status: i32,
    pub payload: Vec<u8>,
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
}

pub struct RingServer<'a, N: Notifier> {
    seg: &'a SharedSeg,
    geom: Geom,
    notifier: N,
}

impl<'a, N: Notifier> RingClient<'a, N> {
    pub fn new(seg: &'a SharedSeg, notifier: N) -> Result<Self, IpcError> {
        let geom = ring::open(seg)?;
        Ok(RingClient { seg, geom, notifier })
    }

    /// Submit a request and block (via the notifier / spin) until the response.
    pub fn submit(&self, opcode: u32, flags: u32, payload: &[u8]) -> Result<Response, IpcError> {
        if payload.len() > self.geom.payload_cap as usize {
            return Err(IpcError::PayloadTooLarge);
        }
        // Claim a free slot (bounded spin so a truly full ring can't hang forever).
        let slot = {
            let mut tries: u32 = 0;
            loop {
                if let Some(s) = ring::claim_free(self.seg, &self.geom) {
                    break s;
                }
                tries = tries.wrapping_add(1);
                if tries > 50_000_000 {
                    return Err(IpcError::RingFull);
                }
                // Spin directly while waiting for any slot to free. (A real
                // Notifier would add a slot-free wait; SpinNotifier just spins.)
                core::hint::spin_loop();
            }
        };
        ring::publish_request(self.seg, &self.geom, slot, opcode, flags, payload)?;
        self.notifier.notify_server();
        let (status, payload) = loop {
            if let Some(r) = ring::take_response(self.seg, &self.geom, slot) {
                break r;
            }
            self.notifier.wait_client(slot);
        };
        ring::free_slot(self.seg, &self.geom, slot)?;
        self.notifier.notify_slot_free();
        Ok(Response { status, payload })
    }
}

impl<'a, N: Notifier> RingServer<'a, N> {
    pub fn new(seg: &'a SharedSeg, notifier: N) -> Result<Self, IpcError> {
        let geom = ring::open(seg)?;
        Ok(RingServer { seg, geom, notifier })
    }

    /// Handle at most one submitted request. Returns Ok(true) if one was handled,
    /// Ok(false) if none was pending (after an advisory `wait_server`).
    pub fn serve_one(
        &self,
        handler: impl FnOnce(&Request) -> (i32, Vec<u8>),
    ) -> Result<bool, IpcError> {
        let slot = match ring::server_take(self.seg, &self.geom) {
            Some(s) => s,
            None => {
                self.notifier.wait_server();
                return Ok(false);
            }
        };
        let (opcode, flags, req_id, payload) =
            ring::read_request(self.seg, &self.geom, slot).ok_or(IpcError::BadResponse)?;
        let req = Request { slot, opcode, flags, req_id, payload };
        let (status, resp) = handler(&req);
        ring::server_complete(self.seg, &self.geom, slot, status, &resp)?;
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
        ring::publish_request(seg, &geom, slot, OP_GETATTR, 3, b"ping").unwrap();

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

        let (status, resp) = ring::take_response(seg, &geom, slot).unwrap();
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
