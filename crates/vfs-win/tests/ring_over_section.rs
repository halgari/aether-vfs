use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use vfs_ipc::layout::OP_GETATTR;
use vfs_ipc::ring::init;
use vfs_ipc::{RingClient, RingServer, SpinNotifier};
use vfs_win::SharedMapping;

fn section_name(tag: &str) -> String {
    let pid = std::process::id();
    format!("Local\\vfs-win-ringtest-{pid}-{tag}")
}

#[test]
fn ring_round_trip_over_real_section() {
    let mapping = SharedMapping::create(&section_name("ring"), 64 * 1024).unwrap();
    init(mapping.seg(), 4, 4096).unwrap();
    let seg = mapping.seg();
    let stop = AtomicBool::new(false);

    thread::scope(|scope| {
        // Server thread: echo each request's payload back until stopped and idle.
        scope.spawn(|| {
            let ring = RingServer::new(seg, SpinNotifier).unwrap();
            loop {
                match ring.serve_one(|req| (0, req.payload.clone())) {
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

        // Client (this thread): submit a request over the real shared section.
        let client = RingClient::new(seg, SpinNotifier).unwrap();
        let resp = client.submit(OP_GETATTR, 0, b"hello-shared-memory").unwrap();
        assert_eq!(resp.status, 0);
        assert_eq!(resp.payload, b"hello-shared-memory");

        stop.store(true, Ordering::Relaxed);
    });
}
