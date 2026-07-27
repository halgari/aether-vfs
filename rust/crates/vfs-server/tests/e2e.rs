use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use vfs_core::{EntryKind, InputEntry, Layer, LayerId};
use vfs_ipc::layout::{OP_GETATTR, OP_HEARTBEAT, OP_READDIR};
use vfs_ipc::ring::init;
use vfs_ipc::{OwnedSeg, RingClient, RingServer, SpinNotifier};
use vfs_server::proto::{decode_getattr_resp, decode_readdir_resp, encode_path_req};
use vfs_server::Server;

fn server() -> Server {
    let e = |vpath: &str, source: &str, size: u64, mtime: i64| InputEntry {
        vpath: vpath.into(),
        kind: EntryKind::File,
        source: source.into(),
        size,
        mtime,
    };
    Server::from_layers(vec![Layer {
        id: LayerId(0),
        entries: vec![e("data/a.esp", "s/a", 10, 1), e("data/b.esp", "s/b", 20, 2)],
    }])
    .unwrap()
}

#[test]
fn client_queries_server_over_ring() {
    let server = server();
    let owned = OwnedSeg::new(64 * 1024);
    init(owned.seg(), 4, 4096).unwrap();
    let seg = owned.seg();
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        // Server thread: drain requests until stopped and idle.
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

        // Client (this thread): issue requests and check responses.
        let client = RingClient::new(seg, SpinNotifier).unwrap();

        let resp = client.submit(OP_GETATTR, 0, &encode_path_req("data/a.esp")).unwrap();
        assert_eq!(resp.status, 0);
        let a = decode_getattr_resp(&resp.payload).unwrap();
        assert!(a.found && !a.is_dir && a.size == 10);

        let resp = client.submit(OP_GETATTR, 0, &encode_path_req("nope")).unwrap();
        assert!(!decode_getattr_resp(&resp.payload).unwrap().found);

        let resp = client.submit(OP_READDIR, 0, &encode_path_req("data")).unwrap();
        assert_eq!(resp.status, 0);
        let names: Vec<String> =
            decode_readdir_resp(&resp.payload).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["a.esp", "b.esp"]);

        // readdir of a file → NOT_A_DIRECTORY
        let resp = client.submit(OP_READDIR, 0, &encode_path_req("data/a.esp")).unwrap();
        assert_eq!(resp.status, -2);

        let resp = client.submit(OP_HEARTBEAT, 0, &[]).unwrap();
        assert_eq!(resp.status, 0);

        stop.store(true, Ordering::Relaxed);
    });
}
