//! Threaded control-ring OPEN/READ/CLOSE against the real Server/Client.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use vfs_core::{EntryKind, InputEntry, Layer, LayerId};
use vfs_ipc::ring::init;
use vfs_ipc::{OwnedSeg, RingClient, RingServer, SpinNotifier};
use vfs_protocol::{
    decode_getattr_resp, decode_open_resp, decode_read_bulk_resp, decode_readdir_resp,
    decode_read_resp, encode_close_req, encode_open_req, encode_path_req, encode_read_req,
    is_read_resp_bulk, OpenResp, ReadReq, FLAG_READ_BULK, OP_CLOSE, OP_GETATTR, OP_OPEN, OP_READ,
    OP_READDIR, OPEN_READ, ST_OK,
};
use vfs_server::{DataArena, Server};

#[test]
fn client_open_read_close_over_ring() {
    let dir = std::env::temp_dir().join(format!("vfs-fuse-e2e-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("f.bin");
    let content = b"fuse-ipc-bytes";
    {
        let mut f = std::fs::File::create(&file).unwrap();
        f.write_all(content).unwrap();
    }
    let src = file.to_string_lossy().into_owned();
    let server = Server::from_layers_with_cap(
        vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/f.bin".into(),
                kind: EntryKind::File,
                source: src.as_str().into(),
                size: content.len() as u64,
                mtime: 1,
            }],
        }],
        4096,
    )
    .unwrap();

    let owned = OwnedSeg::new(64 * 1024);
    init(owned.seg(), 4, 4096).unwrap();
    let seg = owned.seg();
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            let ring = RingServer::new(seg, SpinNotifier).unwrap();
            loop {
                match server.serve_one(&ring) {
                    Ok(true) => {}
                    Ok(false) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let client = RingClient::new(seg, SpinNotifier).unwrap();
        let open_pl = encode_open_req(0, OPEN_READ, "data/f.bin");
        let resp = client.submit(OP_OPEN, 0, &open_pl).unwrap();
        assert_eq!(resp.status, ST_OK);
        let OpenResp { fh, size, .. } = decode_open_resp(&resp.payload).unwrap();
        assert_eq!(size, content.len() as u64);

        let rresp = client
            .submit(
                OP_READ,
                0,
                &encode_read_req(&ReadReq {
                    fh,
                    offset: 0,
                    len: size as u32,
                }),
            )
            .unwrap();
        assert_eq!(rresp.status, ST_OK);
        assert_eq!(decode_read_resp(&rresp.payload).unwrap(), content);

        let cresp = client.submit(OP_CLOSE, 0, &encode_close_req(fh)).unwrap();
        assert_eq!(cresp.status, ST_OK);

        stop.store(true, Ordering::Relaxed);
    });

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn client_getattr_readdir_over_ring() {
    let dir = std::env::temp_dir().join(format!("vfs-fuse-meta-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("a.esp");
    std::fs::write(&file, b"TES4xxxx").unwrap();
    let src = file.to_string_lossy().into_owned();
    let server = Server::from_layers_with_cap(
        vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/a.esp".into(),
                kind: EntryKind::File,
                source: src.as_str().into(),
                size: 8,
                mtime: 1,
            }],
        }],
        4096,
    )
    .unwrap();

    let owned = OwnedSeg::new(64 * 1024);
    init(owned.seg(), 4, 4096).unwrap();
    let seg = owned.seg();
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            let ring = RingServer::new(seg, SpinNotifier).unwrap();
            loop {
                match server.serve_one(&ring) {
                    Ok(true) => {}
                    Ok(false) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let client = RingClient::new(seg, SpinNotifier).unwrap();
        let g = client
            .submit(OP_GETATTR, 0, &encode_path_req(0, "data/a.esp"))
            .unwrap();
        assert_eq!(g.status, ST_OK);
        let a = decode_getattr_resp(&g.payload).unwrap();
        assert!(a.found && !a.is_dir && a.size == 8);

        let d = client
            .submit(OP_READDIR, 0, &encode_path_req(0, "data"))
            .unwrap();
        assert_eq!(d.status, ST_OK);
        let names: Vec<_> = decode_readdir_resp(&d.payload)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["a.esp"]);

        stop.store(true, Ordering::Relaxed);
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// Bulk arena READ: disk → bank via `fill_bank` / `read_into` (no intermediate Vec).
#[test]
fn client_bulk_read_into_arena() {
    let dir = std::env::temp_dir().join(format!("vfs-fuse-bulk-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("big.bin");
    let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&file, &content).unwrap();
    let src = file.to_string_lossy().into_owned();
    let server = Server::from_layers_with_cap(
        vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "data/big.bin".into(),
                kind: EntryKind::File,
                source: src.as_str().into(),
                size: content.len() as u64,
                mtime: 1,
            }],
        }],
        65_536,
    )
    .unwrap();

    const SLOTS: u32 = 4;
    const ARENA: usize = 512 * 1024;
    let stride = ((32 + 65_536) + 7) & !7;
    let ring_bytes = 40 + SLOTS as usize * stride;
    let owned = OwnedSeg::new(ring_bytes + ARENA);
    init(owned.seg(), SLOTS, 65_536).unwrap();
    let seg = owned.seg();
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            let ring = RingServer::new(seg, SpinNotifier).unwrap();
            let arena = DataArena::new(seg, ring_bytes, ARENA, SLOTS as usize);
            loop {
                match server.serve_one_arena(&ring, &arena) {
                    Ok(true) => {}
                    Ok(false) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let client = RingClient::new(seg, SpinNotifier).unwrap();
        let open = client
            .submit(OP_OPEN, 0, &encode_open_req(0, OPEN_READ, "data/big.bin"))
            .unwrap();
        assert_eq!(open.status, ST_OK);
        let OpenResp { fh, size, .. } = decode_open_resp(&open.payload).unwrap();
        assert_eq!(size, content.len() as u64);

        let mut out = Vec::new();
        let mut off = 0u64;
        while off < size {
            let want = ((size - off) as u32).min(128 * 1024);
            let r = client
                .submit(
                    OP_READ,
                    FLAG_READ_BULK,
                    &encode_read_req(&ReadReq {
                        fh,
                        offset: off,
                        len: want,
                    }),
                )
                .unwrap();
            assert_eq!(r.status, ST_OK);
            assert!(is_read_resp_bulk(&r.payload));
            let (n, aoff) = decode_read_bulk_resp(&r.payload).unwrap();
            if n == 0 {
                break;
            }
            let start = out.len();
            out.resize(start + n as usize, 0);
            seg.copy_to(aoff as usize, &mut out[start..]).unwrap();
            off += n as u64;
        }
        assert_eq!(out, content);

        let c = client.submit(OP_CLOSE, 0, &encode_close_req(fh)).unwrap();
        assert_eq!(c.status, ST_OK);
        stop.store(true, Ordering::Relaxed);
    });
    let _ = std::fs::remove_dir_all(&dir);
}
