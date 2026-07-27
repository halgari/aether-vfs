//! Ring opcode dispatch against the userspace FUSE director kernel.

use vfs_protocol::{
    decode_close_req, decode_open_req, decode_path_req, decode_read_req, encode_getattr_resp,
    encode_open_resp, encode_read_resp, encode_read_resp_bulk, encode_readdir_resp, AttrResp,
    DirEntryWire, OpenResp, FLAG_READ_BULK, OP_CLOSE, OP_GETATTR, OP_HEARTBEAT, OP_OPEN, OP_READ,
    OP_READDIR, ST_BAD_REQUEST, ST_NOT_A_DIRECTORY, ST_NOT_FOUND, ST_OK,
};
use vfs_ipc::DataArena;

use crate::director::Director;
use crate::ops::{KIND_DIR, OPEN_READ, OPEN_WRITE};

const BULK_THRESHOLD: u32 = 64 * 1024;

fn max_read_data(payload_cap: u32) -> usize {
    payload_cap.saturating_sub(8) as usize
}

/// Full OPEN/READ/CLOSE + meta against a director kernel.
pub fn dispatch_director(
    director: &Director,
    opcode: u32,
    payload: &[u8],
    flags: u32,
    payload_cap: u32,
    arena: Option<(&DataArena<'_>, u32)>,
) -> (i32, Vec<u8>) {
    match opcode {
        OP_GETATTR => match decode_path_req(payload) {
            Some(vp) => {
                let resp = match director.getattr(&vp) {
                    Ok(Some(s)) => AttrResp {
                        found: true,
                        is_dir: s.kind == KIND_DIR,
                        size: s.size,
                        mtime: s.mtime,
                    },
                    Ok(None) => AttrResp {
                        found: false,
                        is_dir: false,
                        size: 0,
                        mtime: 0,
                    },
                    Err(st) => return (st, Vec::new()),
                };
                (ST_OK, encode_getattr_resp(&resp))
            }
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_READDIR => match decode_path_req(payload) {
            Some(vp) => match director.readdir(&vp) {
                Ok(entries) => {
                    let wire: Vec<DirEntryWire> = entries
                        .into_iter()
                        .map(|e| DirEntryWire {
                            name: e.name,
                            is_dir: e.stat.kind == KIND_DIR,
                            size: e.stat.size,
                            mtime: e.stat.mtime,
                        })
                        .collect();
                    (ST_OK, encode_readdir_resp(&wire))
                }
                Err(st) if st == ST_NOT_A_DIRECTORY => (ST_NOT_A_DIRECTORY, Vec::new()),
                Err(st) if st == ST_NOT_FOUND => (ST_NOT_FOUND, Vec::new()),
                Err(st) => (st, Vec::new()),
            },
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_HEARTBEAT => (ST_OK, Vec::new()),
        OP_OPEN => match decode_open_req(payload) {
            Some((oflags, path)) => {
                if oflags & OPEN_WRITE != 0 {
                    return (ST_BAD_REQUEST, Vec::new());
                }
                let flags = if oflags == 0 { OPEN_READ } else { oflags };
                match director.open(&path, flags) {
                    Ok((fh, size, is_dir)) => {
                        (ST_OK, encode_open_resp(&OpenResp { fh, size, is_dir }))
                    }
                    Err(st) => (st, Vec::new()),
                }
            }
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_READ => match decode_read_req(payload) {
            Some(req) => {
                let want_bulk = (flags & FLAG_READ_BULK) != 0 || req.len >= BULK_THRESHOLD;
                if want_bulk {
                    if let Some((arena, slot)) = arena {
                        let max = arena.bank_size.min(req.len as usize);
                        match arena.fill_bank(slot, max, |buf| {
                            director.read(req.fh, req.offset, buf)
                        }) {
                            Ok((off, n)) => (ST_OK, encode_read_resp_bulk(n as u32, off)),
                            Err(st) => (st, Vec::new()),
                        }
                    } else {
                        let max = max_read_data(payload_cap);
                        let mut buf = vec![0u8; (req.len as usize).min(max)];
                        match director.read(req.fh, req.offset, &mut buf) {
                            Ok(n) => {
                                buf.truncate(n);
                                (ST_OK, encode_read_resp(&buf))
                            }
                            Err(st) => (st, Vec::new()),
                        }
                    }
                } else {
                    let max = max_read_data(payload_cap);
                    let mut buf = vec![0u8; (req.len as usize).min(max)];
                    match director.read(req.fh, req.offset, &mut buf) {
                        Ok(n) => {
                            buf.truncate(n);
                            (ST_OK, encode_read_resp(&buf))
                        }
                        Err(st) => (st, Vec::new()),
                    }
                }
            }
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_CLOSE => match decode_close_req(payload) {
            Some(fh) => match director.close(fh) {
                Ok(()) => (ST_OK, Vec::new()),
                Err(st) => (st, Vec::new()),
            },
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        _ => (ST_BAD_REQUEST, Vec::new()),
    }
}
