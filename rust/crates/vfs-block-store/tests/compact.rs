mod common;

use block_store::CompactOptions;
use common::*;

fn fill(store: &block_store::BlockStore, files: u64) -> Vec<Vec<u8>> {
    (0..files)
        .map(|i| {
            let data = random_bytes(100 + i, 4 * BS);
            let id = format!("f{i}");
            store.set_len(id.as_bytes(), data.len() as u64).unwrap();
            store.write_blocks(id.as_bytes(), 0, &data).unwrap();
            data
        })
        .collect()
}

#[test]
fn compaction_reclaims_space_and_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let data = fill(&store, 40);
    for i in (0..40).filter(|i| i % 4 != 0) {
        store.delete(format!("f{i}").as_bytes()).unwrap();
    }
    let before = store.stats().unwrap();
    let before_bytes: u64 = before.packs.iter().map(|p| p.file_bytes).sum();

    let report = store.compact(CompactOptions::default()).unwrap();
    assert!(report.packs_compacted > 0);

    let after = store.stats().unwrap();
    let after_bytes: u64 = after.packs.iter().map(|p| p.file_bytes).sum();
    assert!(
        after_bytes < before_bytes / 2,
        "{after_bytes} vs {before_bytes}"
    );
    for i in (0..40).filter(|i| i % 4 == 0) {
        assert_eq!(
            read_all(&store, format!("f{i}").as_bytes()),
            data[i as usize]
        );
    }
    assert!(store.verify().unwrap().is_ok());
    let on_disk = std::fs::read_dir(dir.path().join("packs")).unwrap().count();
    assert_eq!(on_disk, after.packs.len());
}

#[test]
fn compaction_respects_threshold_and_budget() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    fill(&store, 40);
    // Nothing deleted: no pack qualifies.
    assert_eq!(
        store
            .compact(CompactOptions::default())
            .unwrap()
            .packs_compacted,
        0
    );
    for i in 0..40 {
        store.delete(format!("f{i}").as_bytes()).unwrap();
    }
    let one = store
        .compact(CompactOptions {
            min_garbage_ratio: 0.5,
            max_bytes: 1,
        })
        .unwrap();
    assert_eq!(one.packs_compacted, 1);
    assert_eq!(one.bytes_moved, 0);
}

#[test]
fn compacted_store_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let data = {
        let store = open(dir.path());
        let data = fill(&store, 20);
        for i in 1..20 {
            store.delete(format!("f{i}").as_bytes()).unwrap();
        }
        store.compact(CompactOptions::default()).unwrap();
        store.close().unwrap();
        data
    };
    let store = open(dir.path());
    assert_eq!(read_all(&store, b"f0"), data[0]);
    assert!(store.verify().unwrap().is_ok());
}

#[test]
fn reads_and_writes_during_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    let data = fill(&store, 60);
    for i in (0..60).filter(|i| i % 3 != 0) {
        store.delete(format!("f{i}").as_bytes()).unwrap();
    }
    std::thread::scope(|s| {
        let store = &store;
        let data = &data;
        for t in 0..3 {
            s.spawn(move || {
                for round in 0..20 {
                    let i = ((round * 3 + t) % 20) * 3;
                    assert_eq!(read_all(store, format!("f{i}").as_bytes()), data[i]);
                }
            });
        }
        s.spawn(move || {
            for i in 0..10u64 {
                let id = format!("new{i}");
                let d = random_bytes(5000 + i, 2 * BS);
                store.set_len(id.as_bytes(), d.len() as u64).unwrap();
                store.write_blocks(id.as_bytes(), 0, &d).unwrap();
            }
        });
        s.spawn(move || {
            store.compact(CompactOptions::default()).unwrap();
        });
    });
    store.compact(CompactOptions::default()).unwrap();
    assert!(store.verify().unwrap().is_ok());
    for i in (0..60).filter(|i| i % 3 == 0) {
        assert_eq!(read_all(&store, format!("f{i}").as_bytes()), data[i]);
    }
}
