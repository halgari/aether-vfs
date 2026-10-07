mod common;

use common::*;
use vfs_block_store::{BlockStore, Error, StoreConfig};

#[test]
fn set_len_creates_and_stat_reports_length() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    assert!(store.stat(b"f").unwrap().is_none());
    store.set_len(b"f", 10 * BS as u64 + 7).unwrap();
    assert_eq!(store.stat(b"f").unwrap().unwrap().len, 10 * BS as u64 + 7);
    assert!(store.cached_ranges(b"f").unwrap().is_empty());
    store.set_len(b"f", 3).unwrap();
    assert_eq!(store.stat(b"f").unwrap().unwrap().len, 3);
}

#[test]
fn delete_removes_file() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    store.set_len(b"f", 100).unwrap();
    store.delete(b"f").unwrap();
    assert!(store.stat(b"f").unwrap().is_none());
    assert!(matches!(store.delete(b"f"), Err(Error::NotFound)));
    assert!(matches!(store.cached_ranges(b"f"), Err(Error::NotFound)));
}

#[test]
fn file_id_length_is_capped() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    assert!(matches!(store.stat(&[0u8; 257]), Err(Error::FileIdTooLong)));
    assert!(matches!(
        store.set_len(&[0u8; 257], 1),
        Err(Error::FileIdTooLong)
    ));
    store.set_len(&[7u8; 256], 1).unwrap();
}

#[test]
fn metadata_survives_close_and_reopen() {
    let dir = vfs_testkit::tempdir().unwrap();
    {
        let store = open(dir.path());
        store.set_len(b"f", 12345).unwrap();
        store.close().unwrap();
    }
    let store = open(dir.path());
    assert_eq!(store.stat(b"f").unwrap().unwrap().len, 12345);
}

#[test]
fn flush_makes_writes_durable_without_close() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    store.set_len(b"f", 99).unwrap();
    store.flush().unwrap();
    assert_eq!(store.stats().unwrap().unflushed_bytes, 0);
    drop(store);
    assert_eq!(open(dir.path()).stat(b"f").unwrap().unwrap().len, 99);
}

#[test]
fn has_unflushed_tracks_writes_since_the_last_flush() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    assert!(!store.has_unflushed(), "fresh store");
    store.set_len(b"f", 99).unwrap();
    assert!(store.has_unflushed(), "after set_len");
    store.flush().unwrap();
    assert!(!store.has_unflushed(), "after flush");
    store.write_blocks(b"f", 0, &[1u8; 99]).unwrap();
    assert!(store.has_unflushed(), "after write_blocks");
    store.flush().unwrap();
    store.delete(b"f").unwrap();
    assert!(store.has_unflushed(), "after delete");
}

#[test]
fn second_open_is_locked() {
    let dir = vfs_testkit::tempdir().unwrap();
    let _store = open(dir.path());
    assert!(matches!(
        BlockStore::open(dir.path(), test_config()),
        Err(Error::Locked)
    ));
}

#[test]
fn block_size_is_fixed_at_creation() {
    let dir = vfs_testkit::tempdir().unwrap();
    open(dir.path()).close().unwrap();
    let cfg = StoreConfig {
        block_size: 8192,
        ..test_config()
    };
    assert!(matches!(
        BlockStore::open(dir.path(), cfg),
        Err(Error::Config(_))
    ));
}

#[test]
fn invalid_config_is_rejected() {
    let dir = vfs_testkit::tempdir().unwrap();
    for cfg in [
        StoreConfig {
            block_size: 1000,
            ..test_config()
        },
        StoreConfig {
            zstd_level: 99,
            ..test_config()
        },
        StoreConfig {
            max_pack_size: BS as u64,
            ..test_config()
        },
        StoreConfig {
            max_file_id_len: 0,
            ..test_config()
        },
        StoreConfig {
            auto_flush_commits: 0,
            ..test_config()
        },
    ] {
        assert!(matches!(
            BlockStore::open(dir.path(), cfg),
            Err(Error::Config(_))
        ));
    }
}

#[test]
fn store_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<BlockStore>();
}

fn pack_path(dir: &std::path::Path, id: u32) -> std::path::PathBuf {
    dir.join("packs").join(format!("{id:08}.pack"))
}

/// Ids of the pack files in the store at `dir`.
fn pack_ids(dir: &std::path::Path) -> Vec<u32> {
    let mut ids: Vec<u32> = std::fs::read_dir(dir.join("packs"))
        .unwrap()
        .filter_map(|e| {
            e.unwrap()
                .file_name()
                .to_str()?
                .strip_suffix(".pack")?
                .parse()
                .ok()
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// A store holding file "a", closed cleanly. Returns a's data.
fn store_with_two_packs(dir: &std::path::Path) -> Vec<u8> {
    let store = open(dir);
    let a = random_bytes(1, 20 * BS);
    store.set_len(b"a", a.len() as u64).unwrap();
    store.write_blocks(b"a", 0, &a).unwrap();
    store.close().unwrap();
    assert!(pack_ids(dir).len() >= 2);
    a
}

/// Writes enough to start new packs, then checks everything reads back.
fn write_more_and_check(store: &BlockStore, a: &[u8]) {
    let b = random_bytes(2, 20 * BS);
    store.set_len(b"b", b.len() as u64).unwrap();
    store.write_blocks(b"b", 0, &b).unwrap();
    assert_eq!(read_all(store, b"a"), a);
    assert_eq!(read_all(store, b"b"), b);
    let report = store.verify().unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
}

#[test]
fn orphan_pack_file_is_deleted_on_open() {
    let dir = vfs_testkit::tempdir().unwrap();
    let a = store_with_two_packs(dir.path());
    let orphan = pack_ids(dir.path()).last().unwrap() + 1;
    std::fs::write(pack_path(dir.path(), orphan), b"not referenced").unwrap();
    let store = open(dir.path());
    assert!(!pack_path(dir.path(), orphan).exists());
    write_more_and_check(&store, &a);
}

/// An orphan pack file that cannot be deleted (held open by another program) must not stop the
/// store from opening, and new packs must not collide with it.
#[cfg(windows)]
#[test]
fn undeletable_orphan_pack_file_does_not_block_open() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = vfs_testkit::tempdir().unwrap();
    let a = store_with_two_packs(dir.path());
    let orphan = pack_ids(dir.path()).last().unwrap() + 1;
    let path = pack_path(dir.path(), orphan);
    std::fs::write(&path, b"not referenced").unwrap();
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&path)
        .unwrap();
    {
        let store = open(dir.path());
        assert!(path.exists());
        write_more_and_check(&store, &a);
        assert!(pack_ids(dir.path()).iter().any(|&id| id > orphan));
    }
    drop(held);
    let store = open(dir.path());
    assert!(!path.exists());
    assert_eq!(read_all(&store, b"a"), a);
    assert!(store.verify().unwrap().is_ok());
}

#[test]
fn auto_flush_after_many_commits() {
    let dir = vfs_testkit::tempdir().unwrap();
    let cfg = StoreConfig {
        auto_flush_commits: 3,
        ..test_config()
    };
    let store = BlockStore::open(dir.path(), cfg).unwrap();
    store.set_len(b"f", 1).unwrap();
    store.set_len(b"f", 2).unwrap();
    assert_eq!(store.stats().unwrap().unflushed_commits, 2);
    store.set_len(b"f", 3).unwrap();
    assert_eq!(store.stats().unwrap().unflushed_commits, 0);
    store.set_len(b"g", 3).unwrap();
    store.delete(b"g").unwrap();
    store.delete(b"f").unwrap();
    assert_eq!(store.stats().unwrap().unflushed_commits, 0);
    // Writes that are all dedup hits append nothing but still count.
    let data = random_bytes(1, BS);
    store.set_len(b"a", BS as u64).unwrap();
    store.write_blocks(b"a", 0, &data).unwrap();
    store.flush().unwrap();
    assert_eq!(store.stats().unwrap().unflushed_commits, 0);
    for _ in 0..3 {
        store.write_blocks(b"a", 0, &data).unwrap();
    }
    let stats = store.stats().unwrap();
    assert_eq!((stats.unflushed_commits, stats.unflushed_bytes), (0, 0));
}

#[test]
fn set_len_rejects_lengths_the_manifest_cannot_hold() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    assert!(matches!(
        store.set_len(b"f", u64::MAX),
        Err(Error::OutOfRange)
    ));
    assert!(store.stat(b"f").unwrap().is_none());
    store.set_len(b"f", 10).unwrap();
    assert!(matches!(
        store.set_len(b"f", u64::MAX),
        Err(Error::OutOfRange)
    ));
    assert_eq!(store.stat(b"f").unwrap().unwrap().len, 10);
}

#[test]
fn file_ids_lists_every_file_once() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store =
        vfs_block_store::BlockStore::open(dir.path(), vfs_block_store::StoreConfig::default())
            .unwrap();
    // A file big enough to span two manifest segments must still be listed once.
    let big = 4096u64 * 64 * 1024 + 1;
    store.set_len(b"b-big", big).unwrap();
    store.set_len(b"a-small", 10).unwrap();
    store.set_len(b"c-gone", 10).unwrap();
    store.delete(b"c-gone").unwrap();
    assert_eq!(
        store.file_ids().unwrap(),
        vec![b"a-small".to_vec(), b"b-big".to_vec()]
    );
}

/// Copies a store directory as it is on disk: what a process killed now leaves.
fn snapshot(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dest = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            snapshot(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), dest).unwrap();
        }
    }
}

/// Only a completed close, `shutdown` or drop counts as a clean shutdown for the next open; a
/// token recorded by `shutdown_with_token` is reported by the next open only, and any later
/// open or plain shutdown replaces it.
#[test]
fn clean_shutdown_and_its_token_are_reported() {
    let dir = vfs_testkit::tempdir().unwrap();
    let store = open(dir.path());
    assert!(!store.opened_after_clean_shutdown(), "a new store");
    assert_eq!(store.clean_shutdown_token(), None);
    store.set_len(b"f", 10).unwrap();
    store.close().unwrap();

    let store = open(dir.path());
    assert!(store.opened_after_clean_shutdown());
    assert_eq!(store.clean_shutdown_token(), None, "a plain close");
    assert!(matches!(
        store.shutdown_with_token(1),
        Err(Error::Config(_))
    ));
    assert!(matches!(
        store.shutdown_with_token(0),
        Err(Error::Config(_))
    ));
    store.shutdown_with_token(0xfeed_beef).unwrap();
    store.shutdown().unwrap(); // only the first does anything
    assert_eq!(store.stat(b"f").unwrap().unwrap().len, 10, "still readable");
    drop(store);

    let store = open(dir.path());
    assert!(store.opened_after_clean_shutdown());
    assert_eq!(store.clean_shutdown_token(), Some(0xfeed_beef));
    drop(store); // a plain clean shutdown replaces the token

    let store = open(dir.path());
    assert!(store.opened_after_clean_shutdown());
    assert_eq!(store.clean_shutdown_token(), None);
    store.set_len(b"g", 5).unwrap();
    store.shutdown_with_token(77).unwrap();
    drop(store);
    #[cfg(not(windows))]
    {
        // Killed while open: the token is gone (the open overwrote it), not clean.
        let store = open(dir.path());
        assert_eq!(store.clean_shutdown_token(), Some(77));
        store.flush().unwrap();
        let killed = vfs_testkit::tempdir().unwrap();
        snapshot(dir.path(), killed.path());
        let k = open(killed.path());
        assert!(!k.opened_after_clean_shutdown());
        assert_eq!(k.clean_shutdown_token(), None);
        assert_eq!(k.stat(b"g").unwrap().unwrap().len, 5);
    }
}
