//! The Director serving over a file-backed ring, with a same-process client.
//!
//! This is the Windows-free half of the Wine path: the mapping is a real file
//! and the notifier is a spin, so nothing here needs an OS event object. Task 4
//! puts the client inside Wine; this pins the server side first.
#![cfg(unix)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use vfs_director::ipc::{clamp_workers, DEFAULT_IO_WORKERS};
use vfs_compose::DiskProvider;
use vfs_director::{DirEntry, Director, Handle, IpcServe, Provider, RootId, Stat};
use vfs_ipc::{RingClient, SpinNotifier};
use vfs_protocol::{
    decode_getattr_resp, decode_open_resp, decode_read_resp, encode_open_req, encode_path_req,
    encode_read_req, ReadReq, OP_GETATTR, OP_OPEN, OP_READ, OPEN_READ, ST_OK,
};
use vfs_provider::{Capabilities, VPath};
use vfs_unix::FileMapping;

/// The vpath the client asks for, and the bytes behind it. Both sides of the
/// ring name the same constants so a drift fails loudly instead of passing on
/// a coincidence.
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

#[test]
fn worker_counts_are_clamped_to_what_the_ring_can_use() {
    assert_eq!(DEFAULT_IO_WORKERS, 4, "the default is unchanged");
    assert_eq!(clamp_workers(0), 1);
    assert_eq!(clamp_workers(12), 12);
    assert_eq!(clamp_workers(1000), 32, "no more workers than ring slots");

    let dir = std::env::temp_dir().join(format!("vfs-serve-fb3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ring = dir.join("ring.bin");
    let d = IpcServe::start_file_backed(Arc::new(Director::new()), &ring, 4096).unwrap();
    assert_eq!(d.worker_count(), DEFAULT_IO_WORKERS);
    // The count is in the ring header, where a client reads it to bound its
    // reads in flight below it: four workers leave three for data.
    assert_eq!(vfs_ipc::ring::worker_hint(d.shared_seg()), 4);
    let gate = vfs_ipc::DataGate::for_ring(d.shared_seg(), &d.client().unwrap().geom());
    assert_eq!(gate.limit(), 3);
    drop(d);
    let w = IpcServe::start_file_backed_with_workers(Arc::new(Director::new()), &ring, 4096, 12)
        .unwrap();
    assert_eq!(w.worker_count(), 12);
    assert_eq!(vfs_ipc::ring::worker_hint(w.shared_seg()), 12);
    let gate = vfs_ipc::DataGate::for_ring(w.shared_seg(), &w.client().unwrap().geom());
    assert_eq!(gate.limit(), 9);
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Opens under `block/` wait on a gate the test opens; everything else is a
/// `DiskProvider`. `blocked` counts opens parked on the gate.
struct Blocking {
    disk: DiskProvider,
    open: Mutex<bool>,
    cv: Condvar,
    blocked: AtomicUsize,
}

impl Blocking {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

impl Provider for Blocking {
    fn capabilities(&self) -> Capabilities {
        self.disk.capabilities()
    }
    fn getattr(&self, p: VPath) -> Result<Option<Stat>, i32> {
        self.disk.getattr(p)
    }
    fn readdir(&self, p: VPath) -> Result<Vec<DirEntry>, i32> {
        self.disk.readdir(p)
    }
    fn open(&self, p: VPath, flags: u32) -> Result<(Handle, u64, bool), i32> {
        if p.rel.starts_with("block/") {
            self.blocked.fetch_add(1, Ordering::SeqCst);
            let mut g = self.open.lock().unwrap();
            while !*g {
                g = self.cv.wait(g).unwrap();
            }
        }
        self.disk.open(p, flags)
    }
    fn close(&self, h: Handle) -> Result<(), i32> {
        self.disk.close(h)
    }
    fn read_at(&self, h: Handle, offset: u64, buf: &mut [u8]) -> Result<usize, i32> {
        self.disk.read_at(h, offset, buf)
    }
}

/// Four requests stuck in a slow provider hold four workers. With the
/// default four that is every worker, and nothing else is answered; with
/// six, a fifth request still is.
#[test]
fn requests_blocked_in_a_provider_do_not_stall_the_ring_when_workers_outnumber_them() {
    const BLOCKERS: usize = 4;
    let dir = std::env::temp_dir().join(format!("vfs-serve-fb4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::create_dir_all(dir.join("block")).unwrap();
    std::fs::write(dir.join("data").join("hello.txt"), CONTENT).unwrap();
    for i in 0..BLOCKERS {
        std::fs::write(dir.join("block").join(format!("{i}")), b"x").unwrap();
    }
    let ring = dir.join("ring.bin");
    let provider = Arc::new(Blocking {
        disk: DiskProvider::new(&dir),
        open: Mutex::new(false),
        cv: Condvar::new(),
        blocked: AtomicUsize::new(0),
    });
    let kernel = Arc::new(Director::new());
    kernel.mount(RootId::DEFAULT, provider.clone()).unwrap();
    let serve =
        IpcServe::start_file_backed_with_workers(kernel, &ring, 4096, BLOCKERS + 2).unwrap();
    let map_bytes = serve.map_bytes;

    let blockers: Vec<_> = (0..BLOCKERS)
        .map(|i| {
            let ring = ring.clone();
            std::thread::spawn(move || {
                let mapping = FileMapping::open(&ring, map_bytes).unwrap();
                let client = RingClient::new(mapping.seg(), SpinNotifier).unwrap();
                let path = format!("block/{i}");
                let o = client.submit(OP_OPEN, 0, &encode_open_req(0, OPEN_READ, &path)).unwrap();
                o.status
            })
        })
        .collect();
    while provider.blocked.load(Ordering::SeqCst) < BLOCKERS {
        std::thread::yield_now();
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let ring2 = ring.clone();
    std::thread::spawn(move || {
        let mapping = FileMapping::open(&ring2, map_bytes).unwrap();
        let client = RingClient::new(mapping.seg(), SpinNotifier).unwrap();
        let g = client.submit(OP_GETATTR, 0, &encode_path_req(0, VPATH)).unwrap();
        let _ = tx.send(g.status);
    });
    let answered = rx.recv_timeout(Duration::from_secs(10));
    provider.release();
    for b in blockers {
        assert_eq!(b.join().unwrap(), ST_OK);
    }
    assert_eq!(
        answered,
        Ok(ST_OK),
        "a getattr must be answered while {BLOCKERS} opens hold {BLOCKERS} of {} workers",
        BLOCKERS + 2
    );
    drop(serve);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The registry generation reaches the ring header every client maps: published when the ring
/// starts, and moved by a registry write before the writer gets its reply. A second client on
/// the same file (another injected process) sees it without asking.
#[test]
fn a_registry_write_moves_the_generation_in_the_ring_header() {
    use vfs_director::registry::RegistryHost;
    use vfs_protocol::{encode_reg_set_value, OP_REG_SET_VALUE};

    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("vfs-serve-fb-reg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ring = dir.join("ring.bin");
    let kernel = Arc::new(Director::new());
    let serve = IpcServe::start_file_backed(kernel.clone(), &ring, 4096).unwrap();

    let other = FileMapping::open(&ring, serve.map_bytes).unwrap();
    let g0 = vfs_ipc::ring::reg_generation(other.seg());
    assert_ne!(g0, 0, "published before the ring is handed out");

    kernel.set_registry(Some(
        RegistryHost::open(Arc::new(vfs_provider::RwMemFixture::new())).unwrap(),
    ));
    let g1 = vfs_ipc::ring::reg_generation(other.seg());
    assert!(g1 > g0, "attaching a layer publishes");

    let client = serve.client().unwrap();
    let r = client
        .submit(
            OP_REG_SET_VALUE,
            0,
            &encode_reg_set_value(r"\Registry\Machine\Software\Mod", "v", 4, &[1, 0, 0, 0]),
        )
        .unwrap();
    assert_eq!(r.status, ST_OK);
    assert!(
        vfs_ipc::ring::reg_generation(other.seg()) > g1,
        "visible to every process by the time the writer has its reply"
    );
    drop(serve);
    let _ = std::fs::remove_dir_all(&dir);
}
