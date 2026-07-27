use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use vfs_ipc::ring::init;
use vfs_ipc::{OwnedSeg, RingClient, RingServer, SpinNotifier};

// Server echoes: status = opcode, response payload = request payload.
fn run(slot_count: u32, payload_cap: u32, client_threads: u32, per_thread: u32) {
    let owned = OwnedSeg::new(64 * 1024);
    init(owned.seg(), slot_count, payload_cap).unwrap();
    let seg = owned.seg();

    let stop = AtomicBool::new(false);
    let handled = AtomicUsize::new(0);

    thread::scope(|scope| {
        // One server thread draining requests until stop AND drained.
        scope.spawn(|| {
            let server = RingServer::new(seg, SpinNotifier).unwrap();
            loop {
                match server.serve_one(|req| (req.opcode as i32, req.payload.clone())) {
                    Ok(true) => {
                        handled.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(false) => {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Client threads: each submits `per_thread` requests with a unique payload
        // and asserts the echo matches exactly (no cross-talk / no torn payloads).
        let mut handles = Vec::new();
        for c in 0..client_threads {
            handles.push(scope.spawn(move || {
                let client = RingClient::new(seg, SpinNotifier).unwrap();
                for i in 0..per_thread {
                    let opcode = 1000 + c;
                    let payload = format!("c{c}-msg{i}").into_bytes();
                    let resp = client.submit(opcode, 0, &payload).unwrap();
                    assert_eq!(resp.status, opcode as i32, "status mismatch");
                    assert_eq!(resp.payload, payload, "echo mismatch / cross-talk");
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // All clients done → all requests were answered (submit blocks until then).
        stop.store(true, Ordering::Relaxed);
    });

    assert_eq!(
        handled.load(Ordering::Relaxed),
        (client_threads * per_thread) as usize,
        "server handled the wrong number of requests"
    );
}

#[test]
fn concurrent_round_trip_no_crosstalk() {
    // Comfortable case: more slots than clients.
    run(8, 256, 4, 2_000);
}

#[test]
fn backpressure_more_clients_than_slots() {
    // Only 2 slots but 8 client threads: submit must spin-claim without deadlock
    // or corruption, and every request is still answered correctly.
    run(2, 128, 8, 500);
}
