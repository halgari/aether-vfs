//! Threaded control-ring OPEN/READ/CLOSE against the real Server/Client.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use vfs_core::{EntryKind, InputEntry, Layer, LayerId};
use vfs_ipc::ring::init;
use vfs_ipc::{OwnedSeg, RingClient, RingServer, SpinNotifier};
use vfs_protocol::{
    decode_getattr_resp, decode_open_resp, decode_readdir_resp, decode_read_resp,
    encode_close_req, encode_open_req, encode_path_req, encode_read_req, OpenResp, ReadReq,
    OP_CLOSE, OP_GETATTR, OP_OPEN, OP_READ, OP_READDIR, OPEN_READ, ST_OK,
};
use vfs_server::Server;

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
        let open_pl = encode_open_req(OPEN_READ, "data/f.bin");
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
            .submit(OP_GETATTR, 0, &encode_path_req("data/a.esp"))
            .unwrap();
        assert_eq!(g.status, ST_OK);
        let a = decode_getattr_resp(&g.payload).unwrap();
        assert!(a.found && !a.is_dir && a.size == 8);

        let d = client
            .submit(OP_READDIR, 0, &encode_path_req("data"))
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
