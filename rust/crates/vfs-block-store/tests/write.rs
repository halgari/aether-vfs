mod common;

use common::*;
use vfs_block_store::{BlockStore, Error};

const RECORD: u64 = 40 + BS as u64; // header + one incompressible block stored raw

fn live_bytes(store: &BlockStore) -> u64 {
    store
        .stats()
        .unwrap()
        .packs
        .iter()
        .map(|p| p.live_bytes)
        .sum()
}

#[test]
fn write_errors() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    assert!(matches!(
        store.write_blocks(b"nope", 0, &[0u8; BS]),
        Err(Error::NotFound)
    ));
    store.set_len(b"f", 2 * BS as u64 + 10).unwrap();
    assert!(matches!(
        store.write_blocks(b"f", 0, &[0u8; 10]),
        Err(Error::Unaligned)
    ));
    assert!(matches!(
        store.write_blocks(b"f", 2, &[0u8; BS]),
        Err(Error::Unaligned)
    ));
    assert!(matches!(
        store.write_blocks(b"f", 3, &[0u8; 1]),
        Err(Error::OutOfRange)
    ));
    store.write_blocks(b"f", 2, &[0u8; 10]).unwrap();
}

#[test]
fn sparse_writes_show_in_cached_ranges() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    let len = 4 * BS as u64 + 10;
    store.set_len(b"s", len).unwrap();
    store.write_blocks(b"s", 1, &random_bytes(2, BS)).unwrap();
    store
        .write_blocks(b"s", 3, &random_bytes(3, BS + 10))
        .unwrap();
    let bs = BS as u64;
    assert_eq!(
        store.cached_ranges(b"s").unwrap(),
        vec![bs..2 * bs, 3 * bs..len]
    );
}

#[test]
fn dedup_within_and_across_files() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    let block = random_bytes(9, BS);
    let data = [block.clone(), block.clone(), block].concat();
    for id in [b"x", b"y"] {
        store.set_len(id, data.len() as u64).unwrap();
        store.write_blocks(id, 0, &data).unwrap();
    }
    assert_eq!(live_bytes(&store), RECORD, "one stored copy");
}

#[test]
fn compressible_blocks_are_stored_compressed() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    store.set_len(b"c", 4 * BS as u64).unwrap();
    store
        .write_blocks(b"c", 0, &pattern_bytes(1, 4 * BS))
        .unwrap();
    assert!(
        live_bytes(&store) < BS as u64,
        "{} bytes",
        live_bytes(&store)
    );
}

#[test]
fn overwrite_frees_the_old_block() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    store.set_len(b"f", BS as u64).unwrap();
    store.write_blocks(b"f", 0, &random_bytes(10, BS)).unwrap();
    store.write_blocks(b"f", 0, &random_bytes(11, BS)).unwrap();
    assert_eq!(live_bytes(&store), RECORD);
    // Rewriting identical content keeps the block.
    store.write_blocks(b"f", 0, &random_bytes(11, BS)).unwrap();
    assert_eq!(live_bytes(&store), RECORD);
}

#[test]
fn delete_and_shrink_free_unshared_blocks_only() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    let shared = random_bytes(12, BS);
    store.set_len(b"a", 3 * BS as u64).unwrap();
    store.set_len(b"b", BS as u64).unwrap();
    store
        .write_blocks(
            b"a",
            0,
            &[shared.clone(), random_bytes(13, BS), random_bytes(14, BS)].concat(),
        )
        .unwrap();
    store.write_blocks(b"b", 0, &shared).unwrap();
    assert_eq!(live_bytes(&store), 3 * RECORD);
    store.set_len(b"a", 2 * BS as u64).unwrap();
    assert_eq!(live_bytes(&store), 2 * RECORD);
    store.delete(b"a").unwrap();
    assert_eq!(live_bytes(&store), RECORD);
    assert_eq!(store.cached_ranges(b"b").unwrap(), vec![0..BS as u64]);
}

#[test]
fn large_write_spans_transactions_segments_and_packs() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    // 4100 blocks: crosses a manifest segment (4096 blocks) and many 64 KiB packs.
    let len = 4100 * BS as u64;
    store.set_len(b"big", len).unwrap();
    store
        .write_blocks(b"big", 0, &random_bytes(14, len as usize))
        .unwrap();
    assert_eq!(store.cached_ranges(b"big").unwrap(), vec![0..len]);
    let stats = store.stats().unwrap();
    assert!(stats.packs.len() > 100);
    assert_eq!(
        stats.packs.iter().filter(|p| !p.sealed).count(),
        1,
        "exactly one active pack"
    );
    assert_eq!(live_bytes(&store), 4100 * RECORD);
}

#[test]
fn concurrent_writers() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    std::thread::scope(|s| {
        for t in 0..4u64 {
            let store = &store;
            s.spawn(move || {
                for i in 0..20u64 {
                    let id = format!("t{t}-{i}");
                    // Seeds overlap between threads, so writers race on the same dedup entries.
                    let data = random_bytes(i, 3 * BS);
                    store.set_len(id.as_bytes(), data.len() as u64).unwrap();
                    store.write_blocks(id.as_bytes(), 0, &data).unwrap();
                }
            });
        }
    });
    for t in 0..4 {
        for i in 0..20 {
            assert_eq!(
                store.cached_ranges(format!("t{t}-{i}").as_bytes()).unwrap(),
                vec![0..3 * BS as u64]
            );
        }
    }
    // 20 seeds x 3 distinct blocks each, shared by all four threads.
    assert_eq!(
        live_bytes(&store),
        60 * RECORD,
        "each distinct block stored once"
    );
}
