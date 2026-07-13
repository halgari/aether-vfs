//! Opcode dispatcher.

use vfs_core::{NodeKind, VfsError, VfsTree};
use vfs_ipc::layout::{OP_GETATTR, OP_HEARTBEAT, OP_READDIR};

use crate::proto::*;

/// Decode a request, query the authoritative tree, encode a response.
/// Pure and total — never panics.
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
                    None => AttrResp { found: false, is_dir: false, size: 0, mtime: 0 },
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
        _ => (ST_BAD_REQUEST, Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};

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

        let (_, p) = dispatch(&t, OP_GETATTR, &encode_path_req("nope"));
        assert!(!decode_getattr_resp(&p).unwrap().found);
    }

    #[test]
    fn readdir_dir_file_and_missing() {
        let t = tree();
        let (st, p) = dispatch(&t, OP_READDIR, &encode_path_req("data"));
        assert_eq!(st, ST_OK);
        let names: Vec<String> =
            decode_readdir_resp(&p).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a.esp", "b.esp"]);

        assert_eq!(dispatch(&t, OP_READDIR, &encode_path_req("data/a.esp")).0, ST_NOT_A_DIRECTORY);
        assert_eq!(dispatch(&t, OP_READDIR, &encode_path_req("nope")).0, ST_NOT_FOUND);
    }

    #[test]
    fn heartbeat_unknown_and_malformed() {
        let t = tree();
        assert_eq!(dispatch(&t, OP_HEARTBEAT, &[]), (ST_OK, Vec::new()));
        assert_eq!(dispatch(&t, 9999, &[]), (ST_BAD_REQUEST, Vec::new()));
        assert_eq!(dispatch(&t, OP_GETATTR, &[0xFF, 0xFE]).0, ST_BAD_REQUEST);
    }
}
