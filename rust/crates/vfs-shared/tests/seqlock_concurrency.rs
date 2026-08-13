use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use vfs_shared::{publish, read_stable, AlignedBuf, SnapshotBuilder};

// A raw pointer wrapper so the writer (needs &mut) and readers (need &) can share
// one buffer across threads — mirroring the cross-process shared-memory reality,
// where Rust's aliasing rules don't apply. All access goes through the seqlock.
#[derive(Clone, Copy)]
struct Shared(*mut u8, usize);
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

fn image(size: u64) -> Vec<u8> {
    let mut b = SnapshotBuilder::new();
    let a = b.add_file("a.esp", b"src/a", size, 1, 0, [0; 32]);
    let root = b.add_dir("", &[("a.esp".into(), a)]);
    b.set_root(root);
    b.finish()
}

#[test]
fn readers_never_see_a_torn_snapshot() {
    let img_a = image(10);
    let img_b = image(9999);
    let cap = img_a.len().max(img_b.len()) + 64;

    let mut buf = AlignedBuf::new(cap);
    publish(buf.as_bytes_mut(), &img_a).unwrap();

    let ptr = Shared(buf.as_bytes_mut().as_mut_ptr(), cap);
    let stop = Arc::new(AtomicBool::new(false));

    // Writer: alternate publishing A and B.
    let writer = {
        let stop = stop.clone();
        thread::spawn(move || {
            let ptr = ptr; // capture the whole Send+Sync wrapper, not ptr.0/ptr.1
            #[allow(unsafe_code)]
            let shared = unsafe { std::slice::from_raw_parts_mut(ptr.0, ptr.1) };
            let mut toggle = false;
            while !stop.load(Ordering::Relaxed) {
                let img = if toggle { &img_a } else { &img_b };
                publish(shared, img).unwrap();
                toggle = !toggle;
            }
        })
    };

    // Readers: each read must observe a size that is exactly one of the two
    // published values — never a torn mixture.
    let mut readers = Vec::new();
    for _ in 0..4 {
        let stop = stop.clone();
        readers.push(thread::spawn(move || {
            let ptr = ptr; // capture the whole Send+Sync wrapper, not ptr.0/ptr.1
            #[allow(unsafe_code)]
            let shared = unsafe { std::slice::from_raw_parts(ptr.0 as *const u8, ptr.1) };
            for _ in 0..20_000 {
                if let Some(Some(sz)) =
                    read_stable(shared, |r| r.getattr(&["a.esp"]).map(|s| s.size))
                {
                    assert!(sz == 10 || sz == 9999, "torn read: size={sz}");
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        }));
    }

    for r in readers {
        r.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
}
