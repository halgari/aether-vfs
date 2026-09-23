mod common;

use block_store::{BlockStore, Error, StoreConfig};
use common::*;

#[test]
fn set_len_creates_and_stat_reports_length() {
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.set_len(b"f", 100).unwrap();
    store.delete(b"f").unwrap();
    assert!(store.stat(b"f").unwrap().is_none());
    assert!(matches!(store.delete(b"f"), Err(Error::NotFound)));
    assert!(matches!(store.cached_ranges(b"f"), Err(Error::NotFound)));
}

#[test]
fn file_id_length_is_capped() {
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path());
    store.set_len(b"f", 99).unwrap();
    store.flush().unwrap();
    assert_eq!(store.stats().unwrap().unflushed_bytes, 0);
    drop(store);
    assert_eq!(open(dir.path()).stat(b"f").unwrap().unwrap().len, 99);
}

#[test]
fn second_open_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let _store = open(dir.path());
    assert!(matches!(
        BlockStore::open(dir.path(), test_config()),
        Err(Error::Locked)
    ));
}

#[test]
fn block_size_is_fixed_at_creation() {
    let dir = tempfile::tempdir().unwrap();
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
    let dir = tempfile::tempdir().unwrap();
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
