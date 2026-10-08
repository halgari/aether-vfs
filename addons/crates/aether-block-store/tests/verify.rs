mod common;

use common::*;

fn populated(dir: &std::path::Path) {
    let store = open(dir);
    let shared = random_bytes(1, BS);
    for (id, extra) in [(&b"a"[..], 2u64), (b"b", 3), (b"c", 4)] {
        store.set_len(id, 3 * BS as u64 + 5).unwrap();
        store
            .write_blocks(
                id,
                0,
                &[shared.clone(), random_bytes(extra, 2 * BS)].concat(),
            )
            .unwrap();
        store
            .write_blocks(id, 3, &random_bytes(extra + 10, 5))
            .unwrap();
    }
    store.write_blocks(b"b", 1, &random_bytes(99, BS)).unwrap();
    store.set_len(b"c", 2 * BS as u64).unwrap();
    store.delete(b"a").unwrap();
    store.close().unwrap();
}

#[test]
fn verify_passes_after_mixed_operations() {
    let dir = vfs_testkit::tempdir().unwrap();
    populated(dir.path());
    let report = open(dir.path()).verify().unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    assert_eq!(report.files, 2);
    // Distinct blocks left: shared, b's rewritten block 1, b's block 2, b's tail, c's block 1.
    assert_eq!(report.blocks, 5);
}

#[test]
fn verify_reports_corrupt_records() {
    let dir = vfs_testkit::tempdir().unwrap();
    populated(dir.path());
    let pack = dir.path().join("packs").join("00000001.pack");
    let mut bytes = std::fs::read(&pack).unwrap();
    bytes[45] ^= 0x01;
    std::fs::write(&pack, bytes).unwrap();
    let report = open(dir.path()).verify().unwrap();
    assert!(!report.is_ok());
    assert!(
        report.problems.iter().any(|p| p.contains("checksum")),
        "{:#?}",
        report.problems
    );
}

#[test]
fn verify_reports_missing_pack_files() {
    let dir = vfs_testkit::tempdir().unwrap();
    populated(dir.path());
    let store = open(dir.path());
    let victim = store
        .stats()
        .unwrap()
        .packs
        .iter()
        .find(|p| p.live_bytes > 0)
        .unwrap()
        .id;
    std::fs::remove_file(dir.path().join("packs").join(format!("{victim:08}.pack"))).unwrap();
    let report = store.verify().unwrap();
    assert!(
        report.problems.iter().any(|p| p.contains("file missing")),
        "{:#?}",
        report.problems
    );
}
