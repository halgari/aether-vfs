//! #[repr(C)] ring framing: headers, offsets, constants.

use core::mem::{align_of, offset_of, size_of};

pub const MAGIC: u32 = 0x5646_4950;
/// Wire-format generation of the ring's *payloads*, not of the ring framing
/// itself — `ring::open` refuses a segment whose version differs, which is the
/// only defence against a stale injected DLL speaking the previous payload
/// shape into a current director.
///
/// - **1** — original: path-carrying payloads were a bare path
///   (`encode_path_req`) or `flags|path` (`encode_open_req`).
/// - **2** — stage 2b task 5: every path-carrying payload gained a leading
///   `root:u32` (see `vfs_protocol::encode_path_req`). A version-1 shim
///   talking to a version-2 director would have the first four bytes of its
///   path read as a root id and the remainder as a truncated path —
///   plausible-looking garbage, never an error. Bumping this turns that into
///   a loud failure at attach.
///
/// **Bump this whenever a payload layout changes.** Opcode numbers are a
/// separate contract and must never be renumbered.
pub const VERSION: u32 = 2;

pub const ST_FREE: u32 = 0;
pub const ST_CLAIMED: u32 = 1;
pub const ST_SUBMITTED: u32 = 2;
pub const ST_PROCESSING: u32 = 3;
pub const ST_COMPLETED: u32 = 4;

// Opcode catalog — reference values; the ring never interprets these.
pub const OP_GETATTR: u32 = 1;
pub const OP_READDIR: u32 = 2;
pub const OP_OPEN: u32 = 3;
pub const OP_MATERIALIZE: u32 = 4;
pub const OP_READ: u32 = 5;
pub const OP_WRITE: u32 = 6;
pub const OP_SETATTR: u32 = 7;
pub const OP_RENAME: u32 = 8;
pub const OP_DELETE: u32 = 9;
pub const OP_MKDIR: u32 = 10;
pub const OP_CLOSE: u32 = 11;
pub const OP_REGISTER_PROCESS: u32 = 12;
pub const OP_HEARTBEAT: u32 = 13;

#[repr(C)]
pub struct RingHeader {
    pub magic: u32,
    pub version: u32,
    pub slot_count: u32,
    pub slot_stride: u32,
    pub payload_cap: u32,
    pub _pad: u32,
    pub req_seq: u64,
    pub submit_seq: u32,
    pub _pad2: u32,
}

#[repr(C)]
pub struct SlotHeader {
    pub state: u32,
    pub opcode: u32,
    pub flags: u32,
    pub payload_len: u32,
    pub status: i32,
    pub _pad: u32,
    pub req_id: u64,
}

pub const RING_HEADER_SIZE: usize = size_of::<RingHeader>();
pub const SLOT_HEADER_SIZE: usize = size_of::<SlotHeader>();

const _: () = assert!(RING_HEADER_SIZE == 40 && align_of::<RingHeader>() == 8);
const _: () = assert!(SLOT_HEADER_SIZE == 32 && align_of::<SlotHeader>() == 8);

pub const RH_MAGIC: usize = offset_of!(RingHeader, magic);
pub const RH_VERSION: usize = offset_of!(RingHeader, version);
pub const RH_SLOT_COUNT: usize = offset_of!(RingHeader, slot_count);
pub const RH_SLOT_STRIDE: usize = offset_of!(RingHeader, slot_stride);
pub const RH_PAYLOAD_CAP: usize = offset_of!(RingHeader, payload_cap);
pub const RH_REQ_SEQ: usize = offset_of!(RingHeader, req_seq);
pub const RH_SUBMIT_SEQ: usize = offset_of!(RingHeader, submit_seq);

pub const SH_STATE: usize = offset_of!(SlotHeader, state);
pub const SH_OPCODE: usize = offset_of!(SlotHeader, opcode);
pub const SH_FLAGS: usize = offset_of!(SlotHeader, flags);
pub const SH_PAYLOAD_LEN: usize = offset_of!(SlotHeader, payload_len);
pub const SH_STATUS: usize = offset_of!(SlotHeader, status);
pub const SH_REQ_ID: usize = offset_of!(SlotHeader, req_id);

/// Round `n` up to a multiple of 8.
pub const fn align8(n: usize) -> usize {
    (n + 7) & !7
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_offsets() {
        assert_eq!(RH_REQ_SEQ, 24);
        assert_eq!(RH_SUBMIT_SEQ, 32);
        assert_eq!(RING_HEADER_SIZE, 40);
    }

    #[test]
    fn slot_offsets() {
        assert_eq!(SH_STATE, 0);
        assert_eq!(SH_STATUS, 16);
        assert_eq!(SH_REQ_ID, 24);
        assert_eq!(SLOT_HEADER_SIZE, 32);
    }

    #[test]
    fn align8_rounds_up() {
        assert_eq!(align8(0), 0);
        assert_eq!(align8(1), 8);
        assert_eq!(align8(32), 32);
        assert_eq!(align8(33), 40);
    }
}
