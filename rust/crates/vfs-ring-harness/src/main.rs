//! Cross-process ring CLIENT: opens a JVM-created section and asserts the JVM
//! server's read-path responses. Exit 0 = all assertions passed.
use std::process::exit;
use vfs_ipc::{RingClient, SpinNotifier};
use vfs_protocol as P;
use vfs_win::SharedMapping;

fn fail(msg: &str) -> ! {
    eprintln!("HARNESS FAIL: {msg}");
    exit(1);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = args
        .get(1)
        .unwrap_or_else(|| fail("usage: vfs-ring-harness <name> <size>"));
    let size: usize = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| fail("bad size"));

    let mapping = SharedMapping::open(name, size).unwrap_or_else(|e| fail(&format!("open: {e}")));
    let client = RingClient::new(mapping.seg(), SpinNotifier)
        .unwrap_or_else(|e| fail(&format!("ring open: {e:?}")));

    // getattr /hello.txt -> size 5
    //
    // Every path-carrying payload leads with the root (see
    // `P::encode_path_req`), GETATTR and READDIR included — a bare path here
    // would be parsed as `root = 0x6C65682F` with the rest of the bytes as
    // the vpath. The OPEN calls below were updated for the root prefix when
    // the wire changed; these two were not, and CI only builds this crate.
    let r = client
        .submit(P::OP_GETATTR, 0, &P::encode_path_req(0, "/hello.txt"))
        .unwrap_or_else(|e| fail(&format!("getattr: {e:?}")));
    let attr = P::decode_getattr_resp(&r.payload).unwrap_or_else(|| fail("getattr decode"));
    if !attr.found || attr.size != 5 {
        fail("getattr /hello.txt wrong");
    }

    // readdir / -> contains hello.txt and big.bin
    let r = client
        .submit(P::OP_READDIR, 0, &P::encode_path_req(0, "/"))
        .unwrap_or_else(|e| fail(&format!("readdir: {e:?}")));
    let entries = P::decode_readdir_resp(&r.payload).unwrap_or_else(|| fail("readdir decode"));
    if !entries.iter().any(|e| e.name == "hello.txt") || !entries.iter().any(|e| e.name == "big.bin") {
        fail("readdir missing entries");
    }

    // open + inline read /hello.txt
    let r = client
        .submit(P::OP_OPEN, P::OPEN_READ, &P::encode_open_req(0, P::OPEN_READ, "/hello.txt"))
        .unwrap_or_else(|e| fail(&format!("open: {e:?}")));
    let op = P::decode_open_resp(&r.payload).unwrap_or_else(|| fail("open decode"));
    let rr = client
        .submit(
            P::OP_READ,
            0,
            &P::encode_read_req(&P::ReadReq { fh: op.fh, offset: 0, len: 5 }),
        )
        .unwrap_or_else(|e| fail(&format!("read: {e:?}")));
    let data = P::decode_read_resp(&rr.payload).unwrap_or_else(|| fail("read decode"));
    if data != b"hello" {
        fail("inline read mismatch");
    }

    // open + BULK read /big.bin (70000 bytes of 'X'); data lands in the arena
    let r = client
        .submit(P::OP_OPEN, P::OPEN_READ, &P::encode_open_req(0, P::OPEN_READ, "/big.bin"))
        .unwrap_or_else(|e| fail(&format!("open big: {e:?}")));
    let op = P::decode_open_resp(&r.payload).unwrap_or_else(|| fail("open big decode"));
    let rr = client
        .submit(
            P::OP_READ,
            P::FLAG_READ_BULK,
            &P::encode_read_req(&P::ReadReq { fh: op.fh, offset: 0, len: 70000 }),
        )
        .unwrap_or_else(|e| fail(&format!("bulk read: {e:?}")));
    let (n, off) = P::decode_read_bulk_resp(&rr.payload).unwrap_or_else(|| fail("expected bulk resp"));
    if n != 70000 {
        fail("bulk length wrong");
    }
    // Read the bytes straight from the arena at `off`.
    let mut buf = vec![0u8; n as usize];
    mapping
        .seg()
        .copy_to(off as usize, &mut buf)
        .unwrap_or_else(|| fail("arena copy_to oob"));
    if buf.iter().any(|&b| b != b'X') {
        fail("bulk arena bytes wrong");
    }

    println!("HARNESS OK");
    exit(0);
}
