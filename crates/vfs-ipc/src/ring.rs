//! Ring init/open + slot state-machine primitives.

use core::sync::atomic::{AtomicU32, Ordering};

use crate::layout::*;
use crate::seg::SharedSeg;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpcError {
    RingFull,
    PayloadTooLarge,
    BadResponse,
    Closed,
    Layout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geom {
    pub slot_count: u32,
    pub slot_stride: u32,
    pub payload_cap: u32,
}

impl Geom {
    pub fn slot_off(&self, slot: u32) -> usize {
        RING_HEADER_SIZE + slot as usize * self.slot_stride as usize
    }
    pub fn payload_off(&self, slot: u32) -> usize {
        self.slot_off(slot) + SLOT_HEADER_SIZE
    }
}

fn state<'a>(seg: &'a SharedSeg, geom: &Geom, slot: u32) -> Option<&'a AtomicU32> {
    seg.atomic_u32(geom.slot_off(slot) + SH_STATE)
}

/// Lay out an empty ring in `seg`. Returns its geometry.
pub fn init(seg: &SharedSeg, slot_count: u32, payload_cap: u32) -> Result<Geom, IpcError> {
    let stride = align8(SLOT_HEADER_SIZE + payload_cap as usize);
    let total = RING_HEADER_SIZE + slot_count as usize * stride;
    if total > seg.len() {
        return Err(IpcError::Layout);
    }
    seg.write_u32(RH_MAGIC, MAGIC);
    seg.write_u32(RH_VERSION, VERSION);
    seg.write_u32(RH_SLOT_COUNT, slot_count);
    seg.write_u32(RH_SLOT_STRIDE, stride as u32);
    seg.write_u32(RH_PAYLOAD_CAP, payload_cap);
    seg.write_u64(RH_REQ_SEQ, 0);
    seg.write_u32(RH_SUBMIT_SEQ, 0);
    let geom = Geom { slot_count, slot_stride: stride as u32, payload_cap };
    for s in 0..slot_count {
        seg.write_u32(geom.slot_off(s) + SH_STATE, ST_FREE);
    }
    Ok(geom)
}

/// Validate an existing ring; return its geometry.
pub fn open(seg: &SharedSeg) -> Result<Geom, IpcError> {
    if seg.len() < RING_HEADER_SIZE {
        return Err(IpcError::Layout);
    }
    if seg.read_u32(RH_MAGIC) != Some(MAGIC) || seg.read_u32(RH_VERSION) != Some(VERSION) {
        return Err(IpcError::Layout);
    }
    let slot_count = seg.read_u32(RH_SLOT_COUNT).ok_or(IpcError::Layout)?;
    let slot_stride = seg.read_u32(RH_SLOT_STRIDE).ok_or(IpcError::Layout)?;
    let payload_cap = seg.read_u32(RH_PAYLOAD_CAP).ok_or(IpcError::Layout)?;
    if slot_stride as usize != align8(SLOT_HEADER_SIZE + payload_cap as usize) {
        return Err(IpcError::Layout);
    }
    let total = RING_HEADER_SIZE as u64 + slot_count as u64 * slot_stride as u64;
    if total > seg.len() as u64 {
        return Err(IpcError::Layout);
    }
    Ok(Geom { slot_count, slot_stride, payload_cap })
}

/// Claim a FREE slot → CLAIMED. Returns the slot index, or None if the ring is full.
pub fn claim_free(seg: &SharedSeg, geom: &Geom) -> Option<u32> {
    for s in 0..geom.slot_count {
        if let Some(st) = state(seg, geom, s) {
            if st
                .compare_exchange(ST_FREE, ST_CLAIMED, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Some(s);
            }
        }
    }
    None
}

/// Write a request into a CLAIMED slot and publish SUBMITTED. Returns the req_id.
pub fn publish_request(
    seg: &SharedSeg,
    geom: &Geom,
    slot: u32,
    opcode: u32,
    flags: u32,
    payload: &[u8],
) -> Result<u64, IpcError> {
    if payload.len() > geom.payload_cap as usize {
        return Err(IpcError::PayloadTooLarge);
    }
    let base = geom.slot_off(slot);
    let req_id = seg
        .atomic_u64(RH_REQ_SEQ)
        .ok_or(IpcError::Layout)?
        .fetch_add(1, Ordering::Relaxed);
    seg.write_u32(base + SH_OPCODE, opcode);
    seg.write_u32(base + SH_FLAGS, flags);
    seg.write_u32(base + SH_PAYLOAD_LEN, payload.len() as u32);
    seg.write_u64(base + SH_REQ_ID, req_id);
    seg.write_bytes(geom.payload_off(slot), payload);
    state(seg, geom, slot)
        .ok_or(IpcError::Layout)?
        .store(ST_SUBMITTED, Ordering::Release);
    seg.atomic_u32(RH_SUBMIT_SEQ)
        .ok_or(IpcError::Layout)?
        .fetch_add(1, Ordering::Relaxed);
    Ok(req_id)
}

/// Server: claim a SUBMITTED slot → PROCESSING. Returns the slot index.
pub fn server_take(seg: &SharedSeg, geom: &Geom) -> Option<u32> {
    for s in 0..geom.slot_count {
        if let Some(st) = state(seg, geom, s) {
            if st
                .compare_exchange(ST_SUBMITTED, ST_PROCESSING, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Some(s);
            }
        }
    }
    None
}

/// Read (opcode, flags, req_id, payload) from a PROCESSING slot.
pub fn read_request(
    seg: &SharedSeg,
    geom: &Geom,
    slot: u32,
) -> Option<(u32, u32, u64, Vec<u8>)> {
    let base = geom.slot_off(slot);
    let opcode = seg.read_u32(base + SH_OPCODE)?;
    let flags = seg.read_u32(base + SH_FLAGS)?;
    let req_id = seg.read_u64(base + SH_REQ_ID)?;
    let len = seg.read_u32(base + SH_PAYLOAD_LEN)? as usize;
    if len > geom.payload_cap as usize {
        return None;
    }
    let payload = seg.read_bytes(geom.payload_off(slot), len)?;
    Some((opcode, flags, req_id, payload))
}

/// Server: write response into a PROCESSING slot and publish COMPLETED.
pub fn server_complete(
    seg: &SharedSeg,
    geom: &Geom,
    slot: u32,
    status: i32,
    resp: &[u8],
) -> Result<(), IpcError> {
    let base = geom.slot_off(slot);
    let (status, len) = if resp.len() > geom.payload_cap as usize {
        (i32::MIN, 0usize) // overflow sentinel; bulk arena deferred
    } else {
        seg.write_bytes(geom.payload_off(slot), resp);
        (status, resp.len())
    };
    seg.write_i32(base + SH_STATUS, status);
    seg.write_u32(base + SH_PAYLOAD_LEN, len as u32);
    state(seg, geom, slot)
        .ok_or(IpcError::Layout)?
        .store(ST_COMPLETED, Ordering::Release);
    Ok(())
}

/// Client: if COMPLETED, read (status, payload). None if not yet completed.
pub fn take_response(seg: &SharedSeg, geom: &Geom, slot: u32) -> Option<(i32, Vec<u8>)> {
    let st = state(seg, geom, slot)?;
    if st.load(Ordering::Acquire) != ST_COMPLETED {
        return None;
    }
    let base = geom.slot_off(slot);
    let status = seg.read_i32(base + SH_STATUS)?;
    let len = seg.read_u32(base + SH_PAYLOAD_LEN)? as usize;
    if len > geom.payload_cap as usize {
        return None;
    }
    let payload = seg.read_bytes(geom.payload_off(slot), len)?;
    Some((status, payload))
}

/// Client: release a slot back to FREE.
pub fn free_slot(seg: &SharedSeg, geom: &Geom, slot: u32) -> Result<(), IpcError> {
    state(seg, geom, slot)
        .ok_or(IpcError::Layout)?
        .store(ST_FREE, Ordering::Release);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seg::OwnedSeg;

    fn ring(slots: u32, cap: u32) -> (OwnedSeg, Geom) {
        // Enough bytes: header + slots*(header+cap rounded).
        let stride = align8(SLOT_HEADER_SIZE + cap as usize);
        let owned = OwnedSeg::new(RING_HEADER_SIZE + slots as usize * stride + 16);
        let geom = init(owned.seg(), slots, cap).unwrap();
        (owned, geom)
    }

    #[test]
    fn init_then_open_roundtrips() {
        let (owned, geom) = ring(4, 64);
        let opened = open(owned.seg()).unwrap();
        assert_eq!(opened.slot_count, geom.slot_count);
        assert_eq!(opened.payload_cap, 64);
        assert_eq!(opened.slot_stride, geom.slot_stride);
    }

    #[test]
    fn open_rejects_bad_magic() {
        let (owned, _) = ring(2, 32);
        owned.seg().write_u32(RH_MAGIC, 0xDEAD);
        assert_eq!(open(owned.seg()), Err(IpcError::Layout));
    }

    #[test]
    fn full_primitive_roundtrip() {
        let (owned, geom) = ring(2, 64);
        let seg = owned.seg();

        let slot = claim_free(seg, &geom).unwrap();
        assert_eq!(slot, 0);
        let req_id = publish_request(seg, &geom, slot, OP_GETATTR, 7, b"hello").unwrap();

        let taken = server_take(seg, &geom).unwrap();
        assert_eq!(taken, slot);
        let (opcode, flags, rid, payload) = read_request(seg, &geom, taken).unwrap();
        assert_eq!(opcode, OP_GETATTR);
        assert_eq!(flags, 7);
        assert_eq!(rid, req_id);
        assert_eq!(payload, b"hello");

        server_complete(seg, &geom, taken, 42, b"world!").unwrap();

        let (status, resp) = take_response(seg, &geom, slot).unwrap();
        assert_eq!(status, 42);
        assert_eq!(resp, b"world!");

        free_slot(seg, &geom, slot).unwrap();
        // Slot is FREE again → claimable.
        assert_eq!(claim_free(seg, &geom).unwrap(), 0);
    }

    #[test]
    fn payload_too_large_is_rejected() {
        let (owned, geom) = ring(1, 8);
        let seg = owned.seg();
        let slot = claim_free(seg, &geom).unwrap();
        assert_eq!(
            publish_request(seg, &geom, slot, OP_READ, 0, b"way too long payload"),
            Err(IpcError::PayloadTooLarge)
        );
    }
}
