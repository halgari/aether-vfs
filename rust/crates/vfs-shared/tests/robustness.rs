use vfs_shared::{SnapshotBuilder, SnapshotReader};

fn fixture() -> Vec<u8> {
    let mut b = SnapshotBuilder::new();
    let a = b.add_file("a.esp", b"src/a", 10, 1, 0, [0; 32]);
    let root = b.add_dir("", &[("a.esp".into(), a)]);
    b.set_root(root);
    b.finish()
}

#[test]
fn truncated_buffers_do_not_panic() {
    let img = fixture();
    for len in 0..img.len() {
        let slice = &img[..len];
        // open may fail; if it succeeds, queries must not panic.
        if let Ok(r) = SnapshotReader::open(slice) {
            let _ = r.getattr(&["a.esp"]);
            let _ = r.resolve(&["a.esp"]);
            let _ = r.readdir(&[]);
        }
    }
}

#[test]
fn single_byte_corruption_never_panics() {
    let base = fixture();
    for i in 0..base.len() {
        for bit in 0..8u8 {
            let mut img = base.clone();
            img[i] ^= 1 << bit;
            if let Ok(r) = SnapshotReader::open(&img) {
                // Navigate a few paths; any bounds error must degrade to None/Err.
                let _ = r.getattr(&[]);
                let _ = r.getattr(&["a.esp"]);
                let _ = r.resolve(&["a.esp"]);
                let _ = r.readdir(&[]);
                let _ = r.readdir(&["a.esp"]);
            }
        }
    }
}
