//! Opcode dispatcher (stateless metadata + stateful open table).

use vfs_core::{NodeKind, VfsError, VfsTree};
use vfs_protocol::{
    decode_close_req, decode_open_req, decode_path_req, decode_read_req, encode_getattr_resp,
    encode_open_resp, encode_path_req, encode_read_resp, encode_readdir_resp, AttrResp,
    DirEntryWire, OP_CLOSE, OP_GETATTR, OP_HEARTBEAT, OP_OPEN, OP_READ, OP_READDIR, ST_BAD_REQUEST,
    ST_NOT_A_DIRECTORY, ST_NOT_FOUND, ST_OK,
};

use crate::open_table::{max_read_data, OpenTable};

/// Decode a request, query the authoritative tree, encode a response.
/// Pure and total for GETATTR/READDIR/HEARTBEAT — never panics.
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
        // Stateful opcodes require dispatch_with_table.
        OP_OPEN | OP_READ | OP_CLOSE => (ST_BAD_REQUEST, Vec::new()),
        _ => (ST_BAD_REQUEST, Vec::new()),
    }
}

/// Full director dispatch including OPEN/READ/CLOSE against `table`.
pub fn dispatch_with_table(
    tree: &VfsTree,
    table: &OpenTable,
    opcode: u32,
    payload: &[u8],
    payload_cap: u32,
) -> (i32, Vec<u8>) {
    match opcode {
        OP_OPEN => match decode_open_req(payload) {
            Some((flags, path)) => match table.open_with_getattr(tree, &path, flags) {
                Ok(r) => (ST_OK, encode_open_resp(&r)),
                Err(st) => (st, Vec::new()),
            },
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_READ => match decode_read_req(payload) {
            Some(req) => {
                let max = max_read_data(payload_cap);
                match table.read(req.fh, req.offset, req.len, max) {
                    Ok(data) => (ST_OK, encode_read_resp(&data)),
                    Err(st) => (st, Vec::new()),
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

// Silence unused import warnings when tests use encode_path_req via re-exports.
#[allow(dead_code)]
fn _keep(p: &str) -> Vec<u8> {
    encode_path_req(p)
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

        let (_, p) = dispatch(&t, OP_GETATTR, &encode_path_req("data"));
        assert!(decode_getattr_resp(&p).unwrap().is_dir);
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
