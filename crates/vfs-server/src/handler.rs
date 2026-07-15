//! Opcode dispatcher (stateless metadata + stateful open table + bulk arena).

use vfs_core::{NodeKind, VfsError, VfsTree};
use vfs_protocol::{
    decode_close_req, decode_open_req, decode_path_req, decode_read_req, encode_getattr_resp,
    encode_open_resp, encode_read_resp, encode_read_resp_bulk, encode_readdir_resp,
    AttrResp, DirEntryWire, FLAG_READ_BULK, OP_CLOSE, OP_GETATTR, OP_HEARTBEAT, OP_OPEN, OP_READ,
    OP_READDIR, ST_BAD_REQUEST, ST_NOT_A_DIRECTORY, ST_NOT_FOUND, ST_OK,
};

use crate::arena::DataArena;
use crate::open_table::{max_read_data, OpenTable};

/// Threshold: READs larger than this prefer bulk arena when available (**B1**).
pub const BULK_THRESHOLD: u32 = 64 * 1024;

pub fn dispatch(tree: &VfsTree, opcode: u32, payload: &[u8]) -> (i32, Vec<u8>) {
    match opcode {
        OP_GETATTR => match decode_path_req(payload) {
            Some(vp) => {
                let resp = match tree.getattr(&vp) {
                    Some(s) => AttrResp {
                        found: true,
                        is_dir: s.kind == NodeKind::Dir,
                        size: s.size,
                        mtime: s.mtime,
                    },
                    None => AttrResp {
                        found: false,
                        is_dir: false,
                        size: 0,
                        mtime: 0,
                    },
                };
                (ST_OK, encode_getattr_resp(&resp))
            }
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_READDIR => match decode_path_req(payload) {
            Some(vp) => match tree.readdir(&vp, None) {
                Ok(entries) => {
                    let wire: Vec<DirEntryWire> = entries
                        .into_iter()
                        .map(|e| DirEntryWire {
                            name: e.name,
                            is_dir: e.kind == NodeKind::Dir,
                            size: e.size,
                            mtime: e.mtime,
                        })
                        .collect();
                    (ST_OK, encode_readdir_resp(&wire))
                }
                Err(VfsError::NotADirectory) => (ST_NOT_A_DIRECTORY, Vec::new()),
                Err(VfsError::NotFound) => (ST_NOT_FOUND, Vec::new()),
            },
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_HEARTBEAT => (ST_OK, Vec::new()),
        OP_OPEN | OP_READ | OP_CLOSE => (ST_BAD_REQUEST, Vec::new()),
        _ => (ST_BAD_REQUEST, Vec::new()),
    }
}

/// Full director dispatch including OPEN/READ/CLOSE.
pub fn dispatch_with_table(
    tree: &VfsTree,
    table: &OpenTable,
    opcode: u32,
    payload: &[u8],
    payload_cap: u32,
) -> (i32, Vec<u8>) {
    dispatch_full(tree, table, opcode, payload, 0, payload_cap, None)
}

/// Dispatch with ring flags + optional bulk arena (**B1**).
pub fn dispatch_full(
    tree: &VfsTree,
    table: &OpenTable,
    opcode: u32,
    payload: &[u8],
    flags: u32,
    payload_cap: u32,
    arena: Option<(&DataArena<'_>, u32 /*slot*/)>,
) -> (i32, Vec<u8>) {
    match opcode {
        OP_OPEN => match decode_open_req(payload) {
            Some((oflags, path)) => match table.open_with_getattr(tree, &path, oflags) {
                Ok(r) => (ST_OK, encode_open_resp(&r)),
                Err(st) => (st, Vec::new()),
            },
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_READ => match decode_read_req(payload) {
            Some(req) => {
                let want_bulk = (flags & FLAG_READ_BULK) != 0 || req.len >= BULK_THRESHOLD;
                if want_bulk {
                    if let Some((arena, slot)) = arena {
                        // **C1:** disk/zip → arena bank directly (no intermediate Vec).
                        let max = arena.bank_size.min(req.len as usize);
                        match arena.fill_bank(slot, max, |buf| {
                            table.read_into(req.fh, req.offset, max, buf)
                        }) {
                            Ok((off, n)) => (ST_OK, encode_read_resp_bulk(n as u32, off)),
                            Err(st) => (st, Vec::new()),
                        }
                    } else {
                        let max = max_read_data(payload_cap);
                        match table.read(req.fh, req.offset, req.len, max) {
                            Ok(data) => (ST_OK, encode_read_resp(&data)),
                            Err(st) => (st, Vec::new()),
                        }
                    }
                } else {
                    let max = max_read_data(payload_cap);
                    match table.read(req.fh, req.offset, req.len, max) {
                        Ok(data) => (ST_OK, encode_read_resp(&data)),
                        Err(st) => (st, Vec::new()),
                    }
                }
            }
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_CLOSE => match decode_close_req(payload) {
            Some(fh) => match table.close(fh) {
                Ok(()) => (ST_OK, Vec::new()),
                Err(st) => (st, Vec::new()),
            },
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        other => dispatch(tree, other, payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
    use vfs_protocol::{decode_getattr_resp, encode_open_req, encode_path_req, OPEN_READ};

    fn tree() -> VfsTree {
        let e = |vpath: &str, source: &str, size: u64, mtime: i64| InputEntry {
            vpath: vpath.into(),
            kind: EntryKind::File,
            source: source.into(),
            size,
            mtime,
        };
        build(vec![Layer {
            id: LayerId(0),
            entries: vec![e("data/a.esp", "s/a", 10, 1), e("data/b.esp", "s/b", 20, 2)],
        }])
        .unwrap()
    }

    #[test]
    fn getattr_hit_dir_and_miss() {
        let t = tree();
        let (st, p) = dispatch(&t, OP_GETATTR, &encode_path_req("data/a.esp"));
        assert_eq!(st, ST_OK);
        let a = decode_getattr_resp(&p).unwrap();
        assert!(a.found && !a.is_dir && a.size == 10 && a.mtime == 1);
    }

    #[test]
    fn open_missing_is_not_found() {
        let t = tree();
        let table = OpenTable::new();
        let (st, _) = dispatch_with_table(
            &t,
            &table,
            OP_OPEN,
            &encode_open_req(OPEN_READ, "nope.bin"),
            65536,
        );
        assert_eq!(st, ST_NOT_FOUND);
    }
}
