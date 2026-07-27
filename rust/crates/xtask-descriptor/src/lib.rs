//! Emits the protocol descriptor (single source of truth for the Clojure mirror).

use std::fmt::Write as _;
use vfs_ipc::layout as L;
use vfs_protocol as P;
use vfs_protocol::{AttrResp, DirEntryWire, ReadReq};

/// FNV-1a over the descriptor text; low 32 bits used as the handshake hash (M3).
pub fn content_hash(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Deterministic EDN descriptor of the wire + ring/arena layout.
pub fn descriptor_edn() -> String {
    let mut s = String::new();
    let body = descriptor_body();
    // Wrap with a self-describing hash of the body so drift is detectable by hash alone.
    let _ = write!(
        s,
        "{{:magic 0x{:08X}\n :version {}\n :content-hash 0x{:08X}\n{}}}\n",
        L::MAGIC,
        L::VERSION,
        content_hash(&body),
        body
    );
    s
}

fn descriptor_body() -> String {
    let mut s = String::new();
    let _ = write!(s, " :opcodes {{");
    for (name, v) in [
        ("getattr", P::OP_GETATTR), ("readdir", P::OP_READDIR), ("open", P::OP_OPEN),
        ("materialize", P::OP_MATERIALIZE), ("read", P::OP_READ), ("write", P::OP_WRITE),
        ("setattr", P::OP_SETATTR), ("rename", P::OP_RENAME), ("delete", P::OP_DELETE),
        ("mkdir", P::OP_MKDIR), ("close", P::OP_CLOSE),
        ("register-process", P::OP_REGISTER_PROCESS), ("heartbeat", P::OP_HEARTBEAT),
    ] {
        let _ = write!(s, ":{name} {v} ");
    }
    let _ = write!(s, "}}\n :statuses {{");
    for (name, v) in [
        ("ok", P::ST_OK), ("not-found", P::ST_NOT_FOUND), ("not-a-directory", P::ST_NOT_A_DIRECTORY),
        ("bad-request", P::ST_BAD_REQUEST), ("io-error", P::ST_IO_ERROR), ("is-dir", P::ST_IS_DIR),
        ("bad-fh", P::ST_BAD_FH), ("no-space", P::ST_NO_SPACE),
    ] {
        let _ = write!(s, ":{name} {v} ");
    }
    let _ = write!(
        s,
        "}}\n :flags {{:open-read {} :open-write {} :read-bulk {} :read-resp-bulk-bit 0x{:08X}}}\n",
        P::OPEN_READ, P::OPEN_WRITE, P::FLAG_READ_BULK, P::READ_RESP_BULK_BIT
    );
    let _ = write!(
        s,
        " :slot-states {{:free {} :claimed {} :submitted {} :processing {} :completed {}}}\n",
        L::ST_FREE, L::ST_CLAIMED, L::ST_SUBMITTED, L::ST_PROCESSING, L::ST_COMPLETED
    );
    let _ = write!(
        s,
        " :ring-header {{:size {} :align 8 :fields {{:magic {} :version {} :slot-count {} :slot-stride {} :payload-cap {} :req-seq {} :submit-seq {}}}}}\n",
        L::RING_HEADER_SIZE, L::RH_MAGIC, L::RH_VERSION, L::RH_SLOT_COUNT, L::RH_SLOT_STRIDE,
        L::RH_PAYLOAD_CAP, L::RH_REQ_SEQ, L::RH_SUBMIT_SEQ
    );
    let _ = write!(
        s,
        " :slot-header {{:size {} :align 8 :fields {{:state {} :opcode {} :flags {} :payload-len {} :status {} :req-id {}}}}}\n",
        L::SLOT_HEADER_SIZE, L::SH_STATE, L::SH_OPCODE, L::SH_FLAGS, L::SH_PAYLOAD_LEN, L::SH_STATUS, L::SH_REQ_ID
    );
    s
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes { let _ = write!(s, "{b:02x}"); }
    s
}

/// Canonical (name, encoded-bytes) vectors. Fixed inputs → exact wire bytes.
pub fn golden_vectors() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("open-req-read-skyrim", P::encode_open_req(P::OPEN_READ, "Data/Skyrim.esm")),
        ("getattr-resp-file-123",
         P::encode_getattr_resp(&AttrResp { found: true, is_dir: false, size: 123, mtime: -7 })),
        ("read-req-fh7-off10-len4",
         P::encode_read_req(&ReadReq { fh: 7, offset: 10, len: 4 })),
        ("read-resp-abcd", P::encode_read_resp(b"abcd")),
        ("readdir-resp-two",
         P::encode_readdir_resp(&[
             DirEntryWire { name: "a.esp".into(), is_dir: false, size: 10, mtime: 1 },
             DirEntryWire { name: "sub".into(),   is_dir: true,  size: 0,  mtime: 0 },
         ])),
        ("close-req-99", P::encode_close_req(99)),
    ]
}

pub fn golden_edn() -> String {
    let mut s = String::from("{:vectors [\n");
    for (name, bytes) in golden_vectors() {
        let _ = write!(s, "  {{:name :{name} :bytes \"{}\"}}\n", hex(&bytes));
    }
    s.push_str("]}\n");
    s
}

#[cfg(test)]
mod golden_tests {
    use super::*;

    #[test]
    fn encoders_match_committed_golden() {
        // The committed file is the contract; regenerating must not change it silently.
        let committed = include_str!("../../../../resources/protocol-golden.edn");
        assert_eq!(golden_edn(), committed,
            "golden vectors drifted — regenerate with bin/regen-protocol and review");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_has_stable_layout_facts() {
        let edn = descriptor_edn();
        assert!(edn.contains(":size 40"), "ring header size");
        assert!(edn.contains(":req-seq 24"));
        assert!(edn.contains(":submit-seq 32"));
        assert!(edn.contains(":read 5"));      // OP_READ
        assert!(edn.contains(":open-write 2"));
        assert!(edn.contains(":not-found -1"));
    }

    #[test]
    fn descriptor_is_deterministic() {
        assert_eq!(descriptor_edn(), descriptor_edn());
    }

    #[test]
    fn hash_changes_with_content() {
        assert_ne!(content_hash("a"), content_hash("b"));
    }
}
