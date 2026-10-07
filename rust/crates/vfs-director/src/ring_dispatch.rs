//! Ring opcode dispatch against the director kernel (the FUSE-style RPC
//! vocabulary; no `/dev/fuse` involved).

use vfs_ipc::DataArena;
use vfs_protocol::{
    decode_close_req, decode_mkdir_req, decode_open_req, decode_path_req, decode_read_req,
    decode_rename_req, decode_setattr_req, decode_write_req, encode_getattr_resp, encode_open_resp,
    encode_read_resp, encode_read_resp_bulk, encode_readdir_resp, encode_write_resp, AttrResp,
    DirEntryWire, OpenResp, RootId, FLAG_READ_BULK, OP_CLOSE, OP_DELETE, OP_GETATTR, OP_HEARTBEAT,
    OP_MKDIR, OP_OPEN, OP_READ, OP_READDIR, OP_REG_CHANGED, OP_REG_LOOKUP, OP_RENAME, OP_SETATTR,
    OP_STORED_NAMES, OP_WRITE, ST_BAD_REQUEST, ST_NOT_SUPPORTED, ST_OK,
};

use crate::director::Director;
use crate::io_stats;
use vfs_provider::{KIND_DIR, OPEN_READ};

mod registry;

use registry::dispatch_registry;

const BULK_THRESHOLD: u32 = 64 * 1024;

/// What a dispatcher returns: the status, then the reply payload.
type Reply = (i32, Vec<u8>);

/// The reply to a request whose payload did not decode.
fn bad() -> Reply {
    (ST_BAD_REQUEST, Vec::new())
}

/// `Ok` carries the reply payload; an error is its status and no payload.
fn reply(r: Result<Vec<u8>, i32>) -> Reply {
    match r {
        Ok(b) => (ST_OK, b),
        Err(st) => (st, Vec::new()),
    }
}

/// A request with no reply payload: success is `ST_OK`, an error its status.
fn status_only(r: Result<(), i32>) -> Reply {
    reply(r.map(|()| Vec::new()))
}

fn max_read_data(payload_cap: u32) -> usize {
    payload_cap.saturating_sub(8) as usize
}

/// Full OPEN/READ/CLOSE + meta against a director kernel.
///
/// **The root now comes off the wire** (stage 2b task 5). It used to be a
/// Rust-level parameter every production caller pinned to `RootId::DEFAULT`,
/// because no ring payload carried a root: the shim could classify a path as
/// belonging to root 1 and had no field in which to say so, so multi-root
/// could not work end to end. Every path-carrying payload
/// (`decode_path_req`/`decode_open_req`/`decode_mkdir_req`/`decode_rename_req`)
/// now leads with a `root:u32`, and this function routes on it.
///
/// Handle-keyed opcodes — READ, WRITE, SETATTR, CLOSE — carry no root and
/// need none: the file handle the director issued at OPEN already identifies
/// which root's provider it came from, so re-stating it would be a second
/// source of truth that could disagree with the first.
pub fn dispatch_director(
    director: &Director,
    opcode: u32,
    payload: &[u8],
    flags: u32,
    payload_cap: u32,
    arena: Option<(&DataArena<'_>, u32)>,
) -> Reply {
    match opcode {
        OP_GETATTR => match decode_path_req(payload) {
            Some((root, vp)) => {
                let root = RootId(root);
                let resp = match director.getattr(root, &vp) {
                    Ok(Some(s)) => {
                        io_stats::record_getattr(&vp, true, false);
                        AttrResp {
                            found: true,
                            is_dir: s.kind == KIND_DIR,
                            size: s.size,
                            mtime: s.mtime,
                        }
                    }
                    Ok(None) => {
                        io_stats::record_getattr(&vp, false, false);
                        AttrResp {
                            found: false,
                            is_dir: false,
                            size: 0,
                            mtime: 0,
                        }
                    }
                    Err(st) => {
                        io_stats::record_getattr(&vp, false, true);
                        return (st, Vec::new());
                    }
                };
                (ST_OK, encode_getattr_resp(&resp))
            }
            None => bad(),
        },
        OP_READDIR => match decode_path_req(payload) {
            Some((root, vp)) => match director.readdir(RootId(root), &vp) {
                Ok(entries) => {
                    io_stats::record_readdir(&vp, true);
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
                Err(st) => {
                    io_stats::record_readdir(&vp, false);
                    (st, Vec::new())
                }
            },
            None => bad(),
        },
        OP_HEARTBEAT => (ST_OK, Vec::new()),
        OP_OPEN => match decode_open_req(payload) {
            Some((root, oflags, path)) => {
                // No blanket rejection of OPEN_WRITE here: `Director::open`
                // is the one place that knows whether the resolved mount's
                // provider can actually serve writes, and it returns
                // `ST_READ_ONLY` when it can't. Gating here too would just
                // duplicate that policy in a place that can't see it.
                let flags = if oflags == 0 { OPEN_READ } else { oflags };
                match director.open_info(RootId(root), &path, flags) {
                    Ok(o) => {
                        io_stats::record_open(&path, Some(o.fh), o.size, false);
                        (
                            ST_OK,
                            encode_open_resp(&OpenResp {
                                fh: o.fh,
                                size: o.size,
                                is_dir: o.is_dir,
                                immutable: o.immutable,
                                mount_gen: o.mount_gen,
                            }),
                        )
                    }
                    Err(st) => {
                        io_stats::record_open(&path, None, 0, true);
                        (st, Vec::new())
                    }
                }
            }
            None => bad(),
        },
        OP_READ => match decode_read_req(payload) {
            Some(req) => {
                let want_bulk = (flags & FLAG_READ_BULK) != 0 || req.len >= BULK_THRESHOLD;
                if let (true, Some((arena, slot))) = (want_bulk, arena) {
                    let max = arena.bank_size.min(req.len as usize);
                    match arena.fill_bank(slot, max, |buf| director.read(req.fh, req.offset, buf)) {
                        Ok((off, n)) => {
                            io_stats::record_read(req.fh, n, false);
                            (ST_OK, encode_read_resp_bulk(n as u32, off))
                        }
                        Err(st) => {
                            io_stats::record_read(req.fh, 0, true);
                            (st, Vec::new())
                        }
                    }
                } else {
                    let max = max_read_data(payload_cap);
                    let mut buf = vec![0u8; (req.len as usize).min(max)];
                    match director.read(req.fh, req.offset, &mut buf) {
                        Ok(n) => {
                            io_stats::record_read(req.fh, n, false);
                            buf.truncate(n);
                            (ST_OK, encode_read_resp(&buf))
                        }
                        Err(st) => {
                            io_stats::record_read(req.fh, 0, true);
                            (st, Vec::new())
                        }
                    }
                }
            }
            None => bad(),
        },
        OP_CLOSE => match decode_close_req(payload) {
            Some(fh) => match director.close(fh) {
                Ok(()) => {
                    io_stats::record_close(fh);
                    (ST_OK, Vec::new())
                }
                Err(st) => (st, Vec::new()),
            },
            None => bad(),
        },
        OP_WRITE => match decode_write_req(payload) {
            Some((req, data)) => match director.write(req.fh, req.offset, &data) {
                Ok(n) => {
                    io_stats::record_write(req.fh, n, false);
                    (ST_OK, encode_write_resp(n as u32))
                }
                Err(st) => {
                    io_stats::record_write(req.fh, 0, true);
                    (st, Vec::new())
                }
            },
            None => bad(),
        },
        // `SetattrReq` is handle-keyed (`fh`, `size`) with no path, so this is
        // "set end-of-file on an open handle" — `Director::set_len`, not the
        // path-keyed `Provider::set_attr`. Do not "fix" this toward
        // `set_attr`; that method has no wire route in this protocol.
        OP_SETATTR => match decode_setattr_req(payload) {
            Some(req) => status_only(director.set_len(req.fh, req.size)),
            None => bad(),
        },
        OP_RENAME => match decode_rename_req(payload) {
            Some((root, from, to)) => status_only(director.rename(RootId(root), &from, &to)),
            None => bad(),
        },
        OP_DELETE => match decode_path_req(payload) {
            Some((root, path)) => status_only(director.remove(RootId(root), &path)),
            None => bad(),
        },
        OP_STORED_NAMES => match vfs_protocol::decode_names_req(payload) {
            Some((root, skip, path)) => {
                match director.stored_names(RootId(root), &path, skip as usize) {
                    Ok(names) => (ST_OK, vfs_protocol::encode_names_resp(&names)),
                    Err(st) => (st, Vec::new()),
                }
            }
            None => bad(),
        },
        OP_MKDIR => match decode_mkdir_req(payload) {
            Some((root, _mode, path)) => status_only(director.mkdir(RootId(root), &path)),
            None => bad(),
        },
        OP_REG_LOOKUP..=OP_REG_CHANGED => match director.registry() {
            Some(host) => dispatch_registry(&host, opcode, payload, payload_cap),
            None => (ST_NOT_SUPPORTED, Vec::new()),
        },
        _ => (ST_BAD_REQUEST, Vec::new()),
    }
}

#[cfg(test)]
mod tests;
