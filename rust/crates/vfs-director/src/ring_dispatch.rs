//! Ring opcode dispatch against the userspace FUSE director kernel.

use vfs_protocol::{
    decode_close_req, decode_mkdir_req, decode_open_req, decode_path_req, decode_read_req,
    decode_rename_req, decode_setattr_req, decode_write_req, encode_getattr_resp,
    encode_open_resp, encode_read_resp, encode_read_resp_bulk, encode_readdir_resp,
    encode_write_resp, AttrResp, DirEntryWire, OpenResp, RootId, FLAG_READ_BULK, OP_CLOSE,
    OP_DELETE, OP_GETATTR, OP_HEARTBEAT, OP_MKDIR, OP_OPEN, OP_READ, OP_READDIR, OP_RENAME,
    OP_SETATTR, OP_WRITE, ST_BAD_REQUEST, ST_NOT_A_DIRECTORY, ST_NOT_FOUND, ST_OK,
};
use vfs_ipc::DataArena;

use crate::director::Director;
use crate::io_stats;
use crate::ops::{KIND_DIR, OPEN_READ};

const BULK_THRESHOLD: u32 = 64 * 1024;

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
) -> (i32, Vec<u8>) {
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
            None => (ST_BAD_REQUEST, Vec::new()),
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
                Err(st) if st == ST_NOT_A_DIRECTORY => {
                    io_stats::record_readdir(&vp, false);
                    (ST_NOT_A_DIRECTORY, Vec::new())
                }
                Err(st) if st == ST_NOT_FOUND => {
                    io_stats::record_readdir(&vp, false);
                    (ST_NOT_FOUND, Vec::new())
                }
                Err(st) => {
                    io_stats::record_readdir(&vp, false);
                    (st, Vec::new())
                }
            },
            None => (ST_BAD_REQUEST, Vec::new()),
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
                match director.open(RootId(root), &path, flags) {
                    Ok((fh, size, is_dir)) => {
                        io_stats::record_open(&path, Some(fh), size, false);
                        (ST_OK, encode_open_resp(&OpenResp { fh, size, is_dir }))
                    }
                    Err(st) => {
                        io_stats::record_open(&path, None, 0, true);
                        (st, Vec::new())
                    }
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
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_CLOSE => match decode_close_req(payload) {
            Some(fh) => match director.close(fh) {
                Ok(()) => {
                    io_stats::record_close(fh);
                    (ST_OK, Vec::new())
                }
                Err(st) => (st, Vec::new()),
            },
            None => (ST_BAD_REQUEST, Vec::new()),
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
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        // `SetattrReq` is handle-keyed (`fh`, `size`) with no path, so this is
        // "set end-of-file on an open handle" — `Director::set_len`, not the
        // path-keyed `Provider::set_attr`. Do not "fix" this toward
        // `set_attr`; that method has no wire route in this protocol.
        OP_SETATTR => match decode_setattr_req(payload) {
            Some(req) => match director.set_len(req.fh, req.size) {
                Ok(()) => (ST_OK, Vec::new()),
                Err(st) => (st, Vec::new()),
            },
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_RENAME => match decode_rename_req(payload) {
            Some((root, from, to)) => match director.rename(RootId(root), &from, &to) {
                Ok(()) => (ST_OK, Vec::new()),
                Err(st) => (st, Vec::new()),
            },
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_DELETE => match decode_path_req(payload) {
            Some((root, path)) => match director.remove(RootId(root), &path) {
                Ok(()) => (ST_OK, Vec::new()),
                Err(st) => (st, Vec::new()),
            },
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        OP_MKDIR => match decode_mkdir_req(payload) {
            Some((root, _mode, path)) => match director.mkdir(RootId(root), &path) {
                Ok(()) => (ST_OK, Vec::new()),
                Err(st) => (st, Vec::new()),
            },
            None => (ST_BAD_REQUEST, Vec::new()),
        },
        _ => (ST_BAD_REQUEST, Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_opcode_round_trips_through_dispatch() {
        use vfs_protocol::{encode_open_req, encode_write_req, decode_open_resp, decode_write_resp,
                           WriteReq, OP_OPEN, OP_WRITE, OPEN_CREATE, OPEN_WRITE, ST_OK};
        let dir = std::env::temp_dir().join(format!("vfs-rdw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let d = Director::new();
        d.mount(RootId::DEFAULT, std::sync::Arc::new(crate::DiskProvider::new(&dir))).unwrap();

        let (st, payload) = dispatch_director(
            &d, OP_OPEN, &encode_open_req(0, OPEN_WRITE | OPEN_CREATE, "w.txt"), 0, 4096, None);
        assert_eq!(st, ST_OK, "open for write must succeed through dispatch");
        let fh = decode_open_resp(&payload).unwrap().fh;

        let req = WriteReq { fh, offset: 0, len: 5 };
        let (st, payload) = dispatch_director(
            &d, OP_WRITE, &encode_write_req(&req, b"hello"), 0, 4096, None);
        assert_eq!(st, ST_OK, "write must succeed through dispatch");
        assert_eq!(decode_write_resp(&payload).unwrap(), 5);

        assert_eq!(std::fs::read(dir.join("w.txt")).unwrap(), b"hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_opcode_removes_the_file() {
        use vfs_protocol::{encode_path_req, OP_DELETE, ST_OK};
        let dir = std::env::temp_dir().join(format!("vfs-rdd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("gone.txt"), b"x").unwrap();
        let d = Director::new();
        d.mount(RootId::DEFAULT, std::sync::Arc::new(crate::DiskProvider::new(&dir))).unwrap();

        let (st, _) = dispatch_director(
            &d, OP_DELETE, &encode_path_req(0, "gone.txt"), 0, 4096, None);
        assert_eq!(st, ST_OK);
        assert!(!dir.join("gone.txt").exists(), "OP_DELETE did not remove the file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_write_against_a_read_only_provider_is_read_only_not_bad_request() {
        use vfs_protocol::{encode_open_req, OP_OPEN, OPEN_WRITE, ST_READ_ONLY};
        let d = Director::new();
        // InlineProvider is Access::Read: the mount itself has no writable
        // backend, so the director (not this dispatch arm) must be the one
        // to say so.
        d.mount(
            RootId::DEFAULT,
            std::sync::Arc::new(vfs_compose::InlineProvider::from_files([(
                "f",
                b"x".as_slice(),
            )])),
        )
        .unwrap();

        let (st, _) = dispatch_director(
            &d, OP_OPEN, &encode_open_req(0, OPEN_WRITE, "f"), 0, 4096, None);
        assert_eq!(
            st, ST_READ_ONLY,
            "OP_OPEN with OPEN_WRITE against a read-only mount must surface ST_READ_ONLY, not a blanket ST_BAD_REQUEST"
        );
    }

    /// Stage 2b task 3, step 1: `[0, "a.txt"]` and `[1, "a.txt"]` resolve to
    /// different content through the director, end to end via
    /// `dispatch_director` — the ring-level counterpart to
    /// `director::tests::two_roots_resolve_the_same_relative_path_independently`.
    /// Updated by task 5: the root is no longer a Rust-level parameter this
    /// test had to supply out of band — it now rides in the OPEN payload
    /// itself, so `encode_open_req(root, …)` is what selects the provider and
    /// `dispatch_director` takes no root argument at all. The subsequent READ
    /// carries none, and needs none: the handle OPEN returned already knows
    /// which root it came from.
    #[test]
    fn different_roots_resolve_the_same_path_to_different_content_via_dispatch() {
        use vfs_protocol::{
            decode_open_resp, decode_read_resp, encode_open_req, encode_read_req, ReadReq,
            OP_OPEN, OP_READ, OPEN_READ, ST_OK,
        };
        let d = Director::new();
        d.mount(
            RootId(0),
            std::sync::Arc::new(vfs_compose::InlineProvider::from_files([(
                "a.txt",
                b"ROOT-ZERO".as_slice(),
            )])),
        )
        .unwrap();
        d.mount(
            RootId(1),
            std::sync::Arc::new(vfs_compose::InlineProvider::from_files([(
                "a.txt",
                b"ROOT-ONE".as_slice(),
            )])),
        )
        .unwrap();

        let read_via = |root: u32| -> Vec<u8> {
            let (st, payload) = dispatch_director(
                &d, OP_OPEN, &encode_open_req(root, OPEN_READ, "a.txt"), 0, 4096, None);
            assert_eq!(st, ST_OK);
            let fh = decode_open_resp(&payload).unwrap().fh;
            let (st, payload) = dispatch_director(
                &d,
                OP_READ,
                &encode_read_req(&ReadReq { fh, offset: 0, len: 64 }),
                0,
                4096,
                None,
            );
            assert_eq!(st, ST_OK);
            decode_read_resp(&payload).unwrap()
        };

        assert_eq!(read_via(0), b"ROOT-ZERO");
        assert_eq!(read_via(1), b"ROOT-ONE");
    }

    /// Stage 2b task 5: the wire itself, not a caller-side parameter, is what
    /// selects the root now. Two OPEN payloads differing **only** in their
    /// leading `root:u32` must reach different providers — which is the whole
    /// end-to-end claim, since `dispatch_director` has no other way left to
    /// learn the root.
    ///
    /// Also pins the failure mode a stale shim would produce: the pre-task-5
    /// OPEN payload was `flags|path`, so feeding those bytes here reads the
    /// flags as a root id and the first four path bytes as flags. That is
    /// caught at ring attach by `vfs_ipc::layout::VERSION`, never here —
    /// asserted below only so the reason the version bump is load-bearing is
    /// recorded next to the code it protects.
    #[test]
    fn the_wire_root_alone_selects_the_provider() {
        use vfs_protocol::{
            decode_open_resp, decode_read_resp, encode_open_req, encode_read_req, ReadReq,
            OP_OPEN, OP_READ, OPEN_READ, ST_OK,
        };
        let d = Director::new();
        for (root, bytes) in [(0u32, b"ZERO".as_slice()), (1u32, b"ONE!".as_slice())] {
            d.mount(
                RootId(root),
                std::sync::Arc::new(vfs_compose::InlineProvider::from_files([("a.txt", bytes)])),
            )
            .unwrap();
        }

        let zero = encode_open_req(0, OPEN_READ, "a.txt");
        let one = encode_open_req(1, OPEN_READ, "a.txt");
        assert_ne!(zero, one, "the two payloads must differ only in the root field");
        assert_eq!(&zero[4..], &one[4..], "…and in nothing else");

        let read = |payload: &[u8]| -> Vec<u8> {
            let (st, resp) = dispatch_director(&d, OP_OPEN, payload, 0, 4096, None);
            assert_eq!(st, ST_OK);
            let fh = decode_open_resp(&resp).unwrap().fh;
            let (st, resp) = dispatch_director(
                &d,
                OP_READ,
                &encode_read_req(&ReadReq { fh, offset: 0, len: 64 }),
                0,
                4096,
                None,
            );
            assert_eq!(st, ST_OK);
            decode_read_resp(&resp).unwrap()
        };
        assert_eq!(read(&zero), b"ZERO");
        assert_eq!(read(&one), b"ONE!");

        // The stale-shim shape: `flags:u32 | path`, i.e. the new encoding with
        // the root field missing. It does not resolve to anything sensible —
        // which is exactly why the ring refuses to attach such a shim rather
        // than letting it reach here.
        let mut stale = OPEN_READ.to_le_bytes().to_vec();
        stale.extend_from_slice(b"a.txt");
        let (st, _) = dispatch_director(&d, OP_OPEN, &stale, 0, 4096, None);
        assert_ne!(st, ST_OK, "a pre-task-5 OPEN payload must not silently succeed");
    }
}
