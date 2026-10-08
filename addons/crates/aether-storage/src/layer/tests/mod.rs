//! Tests of the layer provider, by theme. This file holds the shared helpers.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use vfs_provider::{
    FIXTURE_FILES, KIND_DIR, KIND_FILE, OPEN_CREATE, OPEN_READ, OPEN_TRUNC, OPEN_WRITE, Provider,
    ST_IO_ERROR, SetAttr, VPath,
};

use crate::config::{Durability, StorageConfig};
use crate::ids::layer_file_id;
use crate::storage::Storage;

use super::{LPath, LayerProvider, folded_path};
#[cfg(not(windows))]
use crate::test_util::snapshot_as_killed;

pub(super) const BS: u64 = 4096;

pub(super) fn cfg() -> StorageConfig {
    let mut c = StorageConfig::default();
    c.store.block_size = BS as u32;
    c
}

pub(super) fn temp_storage() -> (Arc<Storage>, tempfile::TempDir) {
    temp_storage_with(Durability::default())
}

/// A storage whose every close, flush and namespace change is a durable
/// point: what tests of those points' guarantees run on.
pub(super) fn temp_storage_every_close() -> (Arc<Storage>, tempfile::TempDir) {
    temp_storage_with(Durability::OnEveryClose)
}

pub(super) fn temp_storage_with(durability: Durability) -> (Arc<Storage>, tempfile::TempDir) {
    let d = vfs_testkit::tempdir().unwrap();
    let s = Storage::open(
        d.path(),
        StorageConfig {
            durability,
            ..cfg()
        },
    )
    .unwrap();
    (s, d)
}

pub(super) fn at(p: &str) -> VPath<'_> {
    VPath::at_default(p)
}

pub(super) fn write_file(p: &Arc<dyn Provider>, rel: &str, off: u64, body: &[u8]) {
    let (h, _, _) = p.open(at(rel), OPEN_WRITE | OPEN_CREATE).unwrap();
    assert_eq!(p.write_at(h, off, body).unwrap(), body.len());
    p.close(h).unwrap();
}

pub(super) fn read_file(p: &Arc<dyn Provider>, rel: &str) -> Vec<u8> {
    let (h, size, is_dir) = p.open(at(rel), OPEN_READ).unwrap();
    assert!(!is_dir);
    let out = read_range(p, h, 0, size as usize);
    p.close(h).unwrap();
    out
}

pub(super) fn read_range(p: &Arc<dyn Provider>, h: u64, off: u64, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut done = 0;
    while done < len {
        let n = p.read_at(h, off + done as u64, &mut out[done..]).unwrap();
        if n == 0 {
            break;
        }
        done += n;
    }
    out.truncate(done);
    out
}

pub(super) fn seed_fixture(p: &Arc<dyn Provider>) {
    p.mkdir(at("sub")).unwrap();
    for (rel, body) in FIXTURE_FILES {
        write_file(p, rel, 0, body);
    }
}

mod concurrency;
mod durability;
mod io;
mod namespace;
mod overlay;
