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
/// - **3** — concurrent clients: the slot state machine gained
///   [`ST_ABANDONED`], the server echoes the request id it answered into
///   [`SlotHeader::ack`], and the ring header carries the server's worker
///   count ([`RingHeader::worker_hint`]). No payload changed shape, but the
///   two ends must agree on who frees a slot whose client stopped waiting: a
///   version-2 server would publish `COMPLETED` over `ABANDONED` and the slot
///   would never be freed, and a version-2 client frees a slot its server is
///   still writing. So the pair is refused at attach, like a payload change.
/// - **4** — registry overlay: the ring header grew [`RingHeader::reg_gen`]
///   (8 bytes, so the slots start 8 bytes later) and the registry opcodes
///   15-22 joined the catalog. A version-3 shim would read its slots 8 bytes
///   off the director's.
///
/// **Bump this whenever a payload layout or the slot state machine changes.**
/// Opcode numbers are a separate contract and must never be renumbered.
pub const VERSION: u32 = 4;

pub const ST_FREE: u32 = 0;
pub const ST_CLAIMED: u32 = 1;
pub const ST_SUBMITTED: u32 = 2;
pub const ST_PROCESSING: u32 = 3;
pub const ST_COMPLETED: u32 = 4;
/// The client stopped waiting for a request a server is still processing.
///
/// The slot stays the server's: nobody may claim it, and its payload and its
/// arena bank may still be written. The server frees it when it finishes
/// (see `ring::server_complete`), which is what keeps a late reply from being
/// read by a later request.
pub const ST_ABANDONED: u32 = 5;

// Opcode catalog: defined once, in `vfs-protocol`; the ring never interprets
// these. Re-exported here so `layout::OP_*` keeps working.
pub use vfs_protocol::{
    OP_CLOSE, OP_DELETE, OP_GETATTR, OP_HEARTBEAT, OP_MKDIR, OP_OPEN, OP_READ, OP_READDIR,
    OP_REG_CHANGED, OP_REG_CREATE_KEY, OP_REG_DELETE_KEY, OP_REG_DELETE_VALUE, OP_REG_KEY,
    OP_REG_LOOKUP, OP_REG_RENAME_KEY, OP_REG_SET_VALUE, OP_RENAME, OP_SETATTR, OP_STORED_NAMES,
    OP_WRITE,
};

#[repr(C)]
pub struct RingHeader {
    pub magic: u32,
    pub version: u32,
    pub slot_count: u32,
    pub slot_stride: u32,
    pub payload_cap: u32,
    /// How many threads serve this ring, written by the server after `init`;
    /// 0 when it did not say. A client bounds its slow requests below this so
    /// that some worker is always left for a fast one.
    pub worker_hint: u32,
    pub req_seq: u64,
    pub submit_seq: u32,
    pub _pad2: u32,
    /// The registry overlay's generation, published by the director for every
    /// process on this ring: it changes on every registry write and whenever
    /// a registry layer is attached or detached, and never goes backwards. 0
    /// means nothing was published. A client may use a registry answer it
    /// cached only while this still reads what it read before asking
    /// (`ring::reg_generation`).
    pub reg_gen: u64,
}

#[repr(C)]
pub struct SlotHeader {
    pub state: u32,
    pub opcode: u32,
    pub flags: u32,
    pub payload_len: u32,
    pub status: i32,
    /// Low 32 bits of the request id this slot's response answers, written by
    /// the server with the response. The client compares it with the id it
    /// published.
    pub ack: u32,
    pub req_id: u64,
}

pub const RING_HEADER_SIZE: usize = size_of::<RingHeader>();
pub const SLOT_HEADER_SIZE: usize = size_of::<SlotHeader>();

const _: () = assert!(RING_HEADER_SIZE == 48 && align_of::<RingHeader>() == 8);
const _: () = assert!(SLOT_HEADER_SIZE == 32 && align_of::<SlotHeader>() == 8);

pub const RH_MAGIC: usize = offset_of!(RingHeader, magic);
pub const RH_VERSION: usize = offset_of!(RingHeader, version);
pub const RH_SLOT_COUNT: usize = offset_of!(RingHeader, slot_count);
pub const RH_SLOT_STRIDE: usize = offset_of!(RingHeader, slot_stride);
pub const RH_PAYLOAD_CAP: usize = offset_of!(RingHeader, payload_cap);
pub const RH_WORKER_HINT: usize = offset_of!(RingHeader, worker_hint);
pub const RH_REQ_SEQ: usize = offset_of!(RingHeader, req_seq);
pub const RH_SUBMIT_SEQ: usize = offset_of!(RingHeader, submit_seq);
pub const RH_REG_GEN: usize = offset_of!(RingHeader, reg_gen);

pub const SH_STATE: usize = offset_of!(SlotHeader, state);
pub const SH_OPCODE: usize = offset_of!(SlotHeader, opcode);
pub const SH_FLAGS: usize = offset_of!(SlotHeader, flags);
pub const SH_PAYLOAD_LEN: usize = offset_of!(SlotHeader, payload_len);
pub const SH_STATUS: usize = offset_of!(SlotHeader, status);
pub const SH_ACK: usize = offset_of!(SlotHeader, ack);
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
        assert_eq!(RH_WORKER_HINT, 20);
        assert_eq!(RH_REQ_SEQ, 24);
        assert_eq!(RH_SUBMIT_SEQ, 32);
        assert_eq!(RH_REG_GEN, 40);
        assert_eq!(RING_HEADER_SIZE, 48);
    }

    #[test]
    fn slot_offsets() {
        assert_eq!(SH_STATE, 0);
        assert_eq!(SH_STATUS, 16);
        assert_eq!(SH_ACK, 20);
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

    /// Every `layout::OP_*` is the vfs-protocol opcode of the same name, and
    /// vfs-protocol lists no opcode the layout lacks.
    #[test]
    fn every_opcode_here_is_the_protocol_opcode() {
        let here: &[(&str, u32)] = &[
            ("getattr", OP_GETATTR),
            ("readdir", OP_READDIR),
            ("open", OP_OPEN),
            ("read", OP_READ),
            ("write", OP_WRITE),
            ("setattr", OP_SETATTR),
            ("rename", OP_RENAME),
            ("delete", OP_DELETE),
            ("mkdir", OP_MKDIR),
            ("close", OP_CLOSE),
            ("heartbeat", OP_HEARTBEAT),
            ("stored-names", OP_STORED_NAMES),
            ("reg-lookup", OP_REG_LOOKUP),
            ("reg-key", OP_REG_KEY),
            ("reg-set-value", OP_REG_SET_VALUE),
            ("reg-delete-value", OP_REG_DELETE_VALUE),
            ("reg-create-key", OP_REG_CREATE_KEY),
            ("reg-delete-key", OP_REG_DELETE_KEY),
            ("reg-rename-key", OP_REG_RENAME_KEY),
            ("reg-changed", OP_REG_CHANGED),
        ];
        assert_eq!(here, vfs_protocol::OPCODES);
    }
}
