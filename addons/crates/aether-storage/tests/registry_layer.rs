//! A `vfs_embed::Session`'s registry layer kept in a [`Storage`]: saves land
//! in the layer, and [`registry_sync_for`] is the session-end durable point
//! (registry overlay spec §5). Moved from `vfs-embed`'s `registry_layer_tests`
//! with the storage; the tests there that need no storage stay, on a memory
//! layer.
#![cfg(feature = "embed")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aether_storage::{Storage, StorageConfig, registry_sync_for};
use vfs_embed::{OPEN_READ, Provider, Session, VPath};

const KEY: &str = r"\Registry\Machine\Software\Mod";

fn read(p: &Arc<dyn Provider>, name: &str) -> Option<Vec<u8>> {
    let (h, _, _) = p.open(VPath::at_default(name), OPEN_READ).ok()?;
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = p.read_at(h, out.len() as u64, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    p.close(h).unwrap();
    Some(out)
}

/// A fresh scratch directory under Cargo's `CARGO_TARGET_TMPDIR` (never `/tmp`).
fn scratch_dir(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "aether-storage-reglayer-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn storage_layer(tag: &str) -> (PathBuf, Arc<dyn Provider>) {
    let dir = scratch_dir(tag);
    let storage = Storage::open(&dir, StorageConfig::default()).unwrap();
    (dir, storage.layer("registry").unwrap())
}

#[test]
fn stop_flushes_and_a_second_save_replaces_the_first() {
    let (dir, layer) = storage_layer("flush");
    let mut s = Session::new();
    s.set_registry_layer(Some(layer.clone()), None).unwrap();
    let host = s.kernel().registry().unwrap();

    host.set_value(KEY, "v", 1, b"one\0").unwrap();
    s.stop_serve();
    let first = read(&layer, "overlay.reg").expect("stop must leave overlay.reg in the layer");
    assert!(first.windows(3).any(|w| w == b"one"));

    // Rename onto an existing overlay.reg, on the provider sessions use.
    host.set_value(KEY, "v", 1, b"two\0").unwrap();
    s.stop_serve();
    let second = read(&layer, "overlay.reg").unwrap();
    assert!(second.windows(3).any(|w| w == b"two"));
    assert!(!second.windows(3).any(|w| w == b"one"));
    assert!(
        read(&layer, "overlay.reg.tmp").is_none(),
        "the temp file must be renamed away"
    );

    // Replacing the layer flushes the old one first.
    host.set_value(KEY, "v", 1, b"six\0").unwrap();
    let (dir2, other) = storage_layer("flush2");
    s.set_registry_layer(Some(other), None).unwrap();
    assert!(
        read(&layer, "overlay.reg")
            .unwrap()
            .windows(3)
            .any(|w| w == b"six")
    );
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(dir2);
}

/// What a process kill at this instant leaves of a storage directory: a
/// copy of its files, taken while the storage is still open. The
/// catalog's non-durable commits live only in the process, so the copy
/// opens as of the last durable point (aether-storage `Durability`), as the
/// directory would after a crash.
///
/// The storage's own crash hook (`crash_on_drop_for_tests`) cannot stand
/// in here: the registry layer's provider runs a durable point when it
/// drops, so dropping the session and the layer before the "crash" would
/// make everything durable whatever the session did.
fn killed_copy(dir: &Path, tag: &str) -> PathBuf {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            let dst = to.join(e.file_name());
            if e.file_type().unwrap().is_dir() {
                copy(&e.path(), &dst);
            } else {
                std::fs::copy(e.path(), dst).unwrap();
            }
        }
    }
    let to = scratch_dir(tag);
    copy(dir, &to);
    to
}

/// The `overlay.reg` a storage directory holds once reopened.
fn reopened_overlay(dir: &Path) -> Option<Vec<u8>> {
    let storage = Storage::open(dir, StorageConfig::default()).unwrap();
    let layer = storage.layer("registry").unwrap();
    let saved = read(&layer, "overlay.reg");
    drop(layer);
    drop(storage);
    saved
}

/// Spec §5's durable point at session end: a registry write, a session
/// stop, then a crash (a process kill, [`killed_copy`]) and a reopen.
/// The write survives because the stop called the storage's sync
/// through the layer's hook.
#[test]
fn a_crash_after_stop_keeps_the_sessions_registry_writes() {
    let dir = scratch_dir("crash");
    let storage = Storage::open(&dir, StorageConfig::default()).unwrap();
    let layer = storage.layer("registry").unwrap();
    let mut s = Session::new();
    s.set_registry_layer(Some(layer), Some(registry_sync_for(&storage)))
        .unwrap();
    s.kernel()
        .registry()
        .unwrap()
        .set_value(KEY, "v", 1, b"kept\0")
        .unwrap();
    s.stop_serve();
    let crashed = killed_copy(&dir, "crash-killed");
    let saved = reopened_overlay(&crashed).expect("the overlay survives the crash");
    assert!(saved.windows(4).any(|w| w == b"kept"));
    drop(s);
    drop(storage);
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(crashed);
}

/// The control for the test above: the same write saved (`flush`) but
/// never synced is lost to the same crash, so the sync is what keeps it.
/// (The layer is attached to a session with no sync hook, which is what
/// opening a `RegistryHost` on it alone did before this test moved here.)
#[test]
fn a_crash_after_a_save_without_the_sync_loses_it() {
    let dir = scratch_dir("crash-control");
    let storage = Storage::open(&dir, StorageConfig::default()).unwrap();
    let layer = storage.layer("registry").unwrap();
    let s = Session::new();
    s.set_registry_layer(Some(layer), None).unwrap();
    let host = s.kernel().registry().unwrap();
    host.set_value(KEY, "v", 1, b"lost\0").unwrap();
    host.flush().unwrap();
    let crashed = killed_copy(&dir, "crash-control-killed");
    assert!(
        reopened_overlay(&crashed).is_none_or(|b| !b.windows(4).any(|w| w == b"lost")),
        "a save with no durable point does not survive the crash"
    );
    drop(host);
    drop(s);
    drop(storage);
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(crashed);
}
