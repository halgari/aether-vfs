//! The Director serving over a file-backed ring, with a same-process client.
//!
//! This is the Windows-free half of the Wine path: the mapping is a real file
//! and the notifier is a spin, so nothing here needs an OS event object. Task 4
//! puts the client inside Wine; this pins the server side first.
#![cfg(unix)]

use std::sync::Arc;

use vfs_director::{Director, DiskProvider, IpcServe, RootId};
use vfs_ipc::{RingClient, SpinNotifier};
use vfs_protocol::{
    decode_getattr_resp, decode_open_resp, decode_read_resp, encode_open_req, encode_path_req,
    encode_read_req, ReadReq, OP_GETATTR, OP_OPEN, OP_READ, OPEN_READ, ST_OK,
};
use vfs_unix::FileMapping;

/// The vpath the client asks for, and the bytes behind it. Both sides of the
/// ring name the same constants so a drift fails loudly instead of passing on
/// a coincidence — the idiom `ring-file-server`/`ring-file-client` already use.
const VPATH: &str = "data/hello.txt";
const CONTENT: &[u8] = b"served-over-a-file-backed-ring\n";

#[test]
fn a_file_backed_serve_answers_a_getattr_and_a_read() {
    let dir = std::env::temp_dir().join(format!("vfs-serve-fb-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // `VPATH` is `data/hello.txt`, and `DiskProvider` maps a vpath straight
    // onto `dir/<vpath>`, so the backing file lives under `dir/data/` — which
    // also keeps the ring file out of the tree being served.
    std::fs::create_dir_all(dir.join("data")).unwrap();
    let backing = dir.join("data").join("hello.txt");
    std::fs::write(&backing, CONTENT).unwrap();
    let ring = dir.join("ring.bin");

    // A Director over one disk-backed entry, built the way
    // `ring_dispatch.rs`'s tests build one: `Director::new()` plus a
    // `DiskProvider` mounted at the default root.
    let kernel = Arc::new(Director::new());
    kernel
        .mount(RootId::DEFAULT, Arc::new(DiskProvider::new(&dir)))
        .unwrap();

    let serve =
        IpcServe::start_file_backed(kernel, &ring, 4096).expect("file-backed serve must start");
    assert_eq!(serve.ring_path(), Some(ring.as_path()));
    assert!(ring.exists(), "the ring file must exist once serving");
    assert!(
        std::fs::metadata(&ring).unwrap().len() >= 2 * 1024 * 1024,
        "the mapping must be fully sized, or a client mmap faults on touch"
    );

    // Drive it with a client over the SAME file, opened independently — that is
    // the property Task 4 depends on. `RingClient::new` runs `ring::open`, so
    // magic, wire version and geometry are validated here rather than assumed.
    let mapping =
        FileMapping::open(&ring, serve.map_bytes).expect("a second mapping of the ring file");
    let client = RingClient::new(mapping.seg(), SpinNotifier).expect("client must attach");
    assert_eq!(
        client.geom().payload_cap, 4096,
        "the client reads geometry out of the header the server wrote"
    );

    let g = client
        .submit(OP_GETATTR, 0, &encode_path_req(0, VPATH))
        .expect("getattr must round-trip");
    assert_eq!(g.status, ST_OK, "getattr status");
    let attr = decode_getattr_resp(&g.payload).expect("getattr decode");
    assert!(attr.found && !attr.is_dir, "{VPATH} must be a found file");
    assert_eq!(attr.size, CONTENT.len() as u64);

    let o = client
        .submit(OP_OPEN, 0, &encode_open_req(0, OPEN_READ, VPATH))
        .expect("open must round-trip");
    assert_eq!(o.status, ST_OK, "open status");
    let fh = decode_open_resp(&o.payload).expect("open decode").fh;

    let r = client
        .submit(
            OP_READ,
            0,
            &encode_read_req(&ReadReq {
                fh,
                offset: 0,
                len: CONTENT.len() as u32,
            }),
        )
        .expect("read must round-trip");
    assert_eq!(r.status, ST_OK, "read status");
    assert_eq!(
        decode_read_resp(&r.payload).expect("read decode"),
        CONTENT,
        "the bytes must come back through the file-backed ring unchanged"
    );

    // Stop the workers before the directory goes: `client` and `mapping` fall
    // out of scope on their own (`RingClient` has no `Drop` to call, and
    // `client` borrows `mapping`, so neither can be dropped by hand here).
    drop(serve);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn starting_twice_on_one_path_does_not_truncate_the_first_ring() {
    // `FileMapping::create` is grow-only precisely so this cannot SIGBUS the
    // first server; assert the file did not shrink.
    let dir = std::env::temp_dir().join(format!("vfs-serve-fb2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ring = dir.join("ring.bin");
    let a = IpcServe::start_file_backed(Arc::new(Director::new()), &ring, 4096).unwrap();
    let len_a = std::fs::metadata(&ring).unwrap().len();
    let b = IpcServe::start_file_backed(Arc::new(Director::new()), &ring, 4096).unwrap();
    assert_eq!(std::fs::metadata(&ring).unwrap().len(), len_a);
    drop(b);
    drop(a);
    let _ = std::fs::remove_dir_all(&dir);
}
