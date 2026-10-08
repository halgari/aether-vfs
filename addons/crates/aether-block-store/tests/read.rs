mod common;

use aether_block_store::{Error, ReadResult};
use common::*;

#[test]
fn write_then_read_whole_file() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    let data = random_bytes(1, 3 * BS + 100);
    store.set_len(b"a", data.len() as u64).unwrap();
    store.write_blocks(b"a", 0, &data).unwrap();
    assert_eq!(read_all(&store, b"a"), data);
}

#[test]
fn sparse_file_reports_missing_ranges() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    let len = 4 * BS + 10;
    store.set_len(b"s", len as u64).unwrap();
    let block1 = random_bytes(2, BS);
    let tail = random_bytes(3, 10);
    store.write_blocks(b"s", 1, &block1).unwrap();
    store.write_blocks(b"s", 4, &tail).unwrap();

    let mut buf = vec![0u8; len];
    let r = store.read(b"s", 0, &mut buf).unwrap();
    let bs = BS as u64;
    assert_eq!(
        r,
        ReadResult {
            bytes: len,
            missing: vec![0..bs, 2 * bs..4 * bs]
        }
    );
    assert_eq!(&buf[BS..2 * BS], &block1[..]);
    assert_eq!(&buf[4 * BS..], &tail[..]);
}

#[test]
fn partial_and_clamped_reads() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    let data = pattern_bytes(7, 2 * BS + 5);
    store.set_len(b"p", data.len() as u64).unwrap();
    store.write_blocks(b"p", 0, &data).unwrap();

    let mut buf = vec![0u8; 100];
    let r = store.read(b"p", BS as u64 - 50, &mut buf).unwrap();
    assert_eq!(r.bytes, 100);
    assert_eq!(&buf[..], &data[BS - 50..BS + 50]);

    let mut buf = vec![0u8; 1000];
    let r = store.read(b"p", 2 * BS as u64, &mut buf).unwrap();
    assert_eq!(r.bytes, 5);
    assert_eq!(&buf[..5], &data[2 * BS..]);

    assert_eq!(store.read(b"p", 10 * BS as u64, &mut buf).unwrap().bytes, 0);
    assert!(matches!(
        store.read(b"nope", 0, &mut buf),
        Err(Error::NotFound)
    ));
}

#[test]
fn reads_across_manifest_segments() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    let data = pattern_bytes(8, 4100 * BS);
    store.set_len(b"big", data.len() as u64).unwrap();
    store.write_blocks(b"big", 0, &data).unwrap();
    let mut buf = vec![0u8; 3 * BS];
    let offset = 4095 * BS - 17;
    store.read(b"big", offset as u64, &mut buf).unwrap();
    assert_eq!(buf, data[offset..offset + 3 * BS]);
}

#[test]
fn data_survives_reopen_and_appending_resumes() {
    let dir = vfs_testkit::tempdir().unwrap();
    let data = pattern_bytes(15, 5 * BS);
    {
        let store = open(dir.path());
        store.set_len(b"f", data.len() as u64).unwrap();
        store.write_blocks(b"f", 0, &data).unwrap();
        store.close().unwrap();
    }
    let store = open(dir.path());
    assert_eq!(read_all(&store, b"f"), data);
    let packs_before = store.stats().unwrap().packs.len();
    let g = random_bytes(16, BS);
    store.set_len(b"g", BS as u64).unwrap();
    store.write_blocks(b"g", 0, &g).unwrap();
    assert_eq!(
        store.stats().unwrap().packs.len(),
        packs_before,
        "clean reopen resumes the active pack"
    );
    assert_eq!(read_all(&store, b"g"), g);
}

#[test]
fn corrupt_block_heals_to_missing() {
    let dir = vfs_testkit::tempdir().unwrap();
    let data = random_bytes(17, 2 * BS);
    {
        let store = open(dir.path());
        store.set_len(b"f", data.len() as u64).unwrap();
        store.write_blocks(b"f", 0, &data).unwrap();
        store.close().unwrap();
    }
    // Flip the first payload byte of the first record.
    let pack = dir.path().join("packs").join("00000001.pack");
    let mut bytes = std::fs::read(&pack).unwrap();
    bytes[40] ^= 0xff;
    std::fs::write(&pack, bytes).unwrap();

    let store = open(dir.path());
    let mut buf = vec![0u8; data.len()];
    let r = store.read(b"f", 0, &mut buf).unwrap();
    assert_eq!(r.missing, vec![0..BS as u64]);
    assert_eq!(&buf[BS..], &data[BS..]);
    assert_eq!(store.stats().unwrap().healed_blocks, 1);
    // Still missing on the next read, and rewriting restores it.
    assert_eq!(
        store.read(b"f", 0, &mut buf).unwrap().missing,
        vec![0..BS as u64]
    );
    store.write_blocks(b"f", 0, &data[..BS]).unwrap();
    assert_eq!(read_all(&store, b"f"), data);
}

#[test]
fn concurrent_readers_and_writers() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    std::thread::scope(|s| {
        for t in 0..4u64 {
            let store = &store;
            s.spawn(move || {
                for i in 0..20u64 {
                    let id = format!("t{t}-{i}");
                    let data = random_bytes(t * 1000 + i, 3 * BS);
                    store.set_len(id.as_bytes(), data.len() as u64).unwrap();
                    store.write_blocks(id.as_bytes(), 0, &data).unwrap();
                    assert_eq!(read_all(store, id.as_bytes()), data);
                }
            });
        }
    });
}

#[test]
fn corrupt_shared_block_counts_one_heal() {
    let dir = vfs_testkit::tempdir().unwrap();
    let b = random_bytes(21, BS);
    let data = [b.clone(), b].concat();
    {
        let store = open(dir.path());
        store.set_len(b"f", data.len() as u64).unwrap();
        store.write_blocks(b"f", 0, &data).unwrap();
        store.close().unwrap();
    }
    // Flip the first payload byte of the first record.
    let pack = dir.path().join("packs").join("00000001.pack");
    let mut bytes = std::fs::read(&pack).unwrap();
    bytes[40] ^= 0xff;
    std::fs::write(&pack, bytes).unwrap();

    let store = open(dir.path());
    let mut buf = vec![0u8; data.len()];
    let r = store.read(b"f", 0, &mut buf).unwrap();
    assert_eq!(r.missing, vec![0..2 * BS as u64]);
    assert_eq!(store.stats().unwrap().healed_blocks, 1);
}
