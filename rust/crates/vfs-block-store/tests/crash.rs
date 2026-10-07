//! Crash tests: a child process runs a scenario and aborts at a named crash point; the parent
//! reopens the store and checks that flushed data survived and nothing reads back wrong.
//! Run with: cargo test --features crash-points --test crash
#![cfg(feature = "crash-points")]

mod common;

use std::path::Path;
use std::process::Command;

use common::*;
use vfs_block_store::{BlockStore, CompactOptions};

const ROLE: &str = "BLOCK_STORE_CRASH_ROLE";
const DIR: &str = "BLOCK_STORE_CRASH_DIR";
/// Crash point the child arms once its setup is done.
const POINT: &str = "BLOCK_STORE_CRASH_POINT";
/// Read by the library's crash points.
const CRASH_AT: &str = "BLOCK_STORE_CRASH_AT";

/// Durable baseline: file "a" (flushed). Then unflushed file "b".
fn setup(store: &BlockStore) {
    let a = random_bytes(1, 6 * BS);
    store.set_len(b"a", a.len() as u64).unwrap();
    store.write_blocks(b"a", 0, &a).unwrap();
    store.flush().unwrap();
}

fn scenario(name: &str, dir: &Path) {
    let store = if name == "autoflush" {
        // Any write past a block makes the store flush on its own.
        BlockStore::open(
            dir,
            vfs_block_store::StoreConfig {
                auto_flush_bytes: 2 * BS as u64,
                ..test_config()
            },
        )
        .unwrap()
    } else {
        open(dir)
    };
    setup(&store);
    // SAFETY: the child runs a single test thread and no other thread reads the environment
    // concurrently; crash points only read this variable.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var(CRASH_AT, std::env::var(POINT).unwrap())
    };
    match name {
        "write" => {
            let b = random_bytes(2, 3 * BS);
            store.set_len(b"b", b.len() as u64).unwrap();
            store.write_blocks(b"b", 0, &b).unwrap();
        }
        "flush" => {
            let b = random_bytes(2, 3 * BS);
            store.set_len(b"b", b.len() as u64).unwrap();
            store.write_blocks(b"b", 0, &b).unwrap();
            store.flush().unwrap();
        }
        "autoflush" => {
            // The flush comes from `maybe_auto_flush` (lock, check again, flush).
            let b = random_bytes(2, 3 * BS);
            store.set_len(b"b", b.len() as u64).unwrap();
            store.write_blocks(b"b", 0, &b).unwrap();
        }
        "compact" => {
            // Fill several packs, delete most of it, compact.
            for i in 0..30u64 {
                let id = format!("junk{i}");
                store.set_len(id.as_bytes(), 4 * BS as u64).unwrap();
                store
                    .write_blocks(id.as_bytes(), 0, &random_bytes(100 + i, 4 * BS))
                    .unwrap();
            }
            store.flush().unwrap();
            for i in 0..30u64 {
                store.delete(format!("junk{i}").as_bytes()).unwrap();
            }
            store.compact(CompactOptions::default()).unwrap();
        }
        other => panic!("unknown scenario {other}"),
    }
    // Scenarios must crash before reaching here.
    std::process::exit(3);
}

/// Runs `scenario` in a child process that aborts at `point`, then returns the store directory.
fn crash_child(scenario: &str, point: &str) -> tempfile::TempDir {
    let dir = vfs_testkit::tempdir().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_entry", "--nocapture", "--test-threads=1"])
        .env(ROLE, scenario)
        .env(DIR, dir.path())
        .env(POINT, point)
        .env_remove(CRASH_AT)
        .status()
        .unwrap();
    assert!(!status.success(), "child should have aborted at {point}");
    assert_ne!(
        status.code(),
        Some(3),
        "child never reached crash point {point}"
    );
    dir
}

/// Entry point for the child process; does nothing in a normal test run.
#[test]
fn child_entry() {
    if let (Ok(role), Ok(dir)) = (std::env::var(ROLE), std::env::var(DIR)) {
        scenario(&role, Path::new(&dir));
    }
}

fn check_after_crash(dir: &Path) -> BlockStore {
    let store = open(dir);
    assert_eq!(
        read_all(&store, b"a"),
        random_bytes(1, 6 * BS),
        "flushed data lost"
    );
    let report = store.verify().unwrap();
    assert!(report.is_ok(), "{:#?}", report.problems);
    store
}

#[test]
fn crash_after_append_before_commit() {
    let dir = crash_child("write", "write_after_append");
    let store = check_after_crash(dir.path());
    // "b" may exist (set_len commits are not durable, so usually not); if it does, it must not
    // return wrong bytes.
    if let Some(info) = store.stat(b"b").unwrap() {
        let mut buf = vec![0u8; info.len as usize];
        let r = store.read(b"b", 0, &mut buf).unwrap();
        let b = random_bytes(2, 3 * BS);
        for block in 0..3 {
            let range = (block * BS) as u64..((block + 1) * BS) as u64;
            if !r
                .missing
                .iter()
                .any(|m| m.start <= range.start && range.end <= m.end)
            {
                assert!(buf[block * BS..(block + 1) * BS] == b[block * BS..(block + 1) * BS]);
            }
        }
    }
}

#[test]
fn crash_during_flush_before_durable_commit() {
    let dir = crash_child("flush", "flush_before_commit");
    let store = check_after_crash(dir.path());
    // The durable commit never happened, so "b" is not there.
    assert!(store.stat(b"b").unwrap().is_none());
}

#[test]
fn crash_during_an_auto_flush_before_its_durable_commit() {
    let dir = crash_child("autoflush", "flush_before_commit");
    let store = check_after_crash(dir.path());
    assert!(store.stat(b"b").unwrap().is_none());
}

#[test]
fn crash_during_compaction_after_copy() {
    let dir = crash_child("compact", "compact_after_copy");
    check_after_crash(dir.path());
}

#[test]
fn crash_during_compaction_before_retire() {
    let dir = crash_child("compact", "compact_before_retire");
    check_after_crash(dir.path());
}

#[test]
fn crash_during_compaction_after_retire() {
    let dir = crash_child("compact", "compact_after_retire");
    let store = check_after_crash(dir.path());
    // The retired pack was deleted on open.
    let on_disk = std::fs::read_dir(dir.path().join("packs")).unwrap().count();
    assert_eq!(on_disk, store.stats().unwrap().packs.len());
}

#[test]
fn store_is_usable_after_crash() {
    let dir = crash_child("write", "write_after_append");
    let store = check_after_crash(dir.path());
    let c = random_bytes(3, 2 * BS);
    store.set_len(b"c", c.len() as u64).unwrap();
    store.write_blocks(b"c", 0, &c).unwrap();
    store.flush().unwrap();
    drop(store);
    let store = check_after_crash(dir.path());
    assert_eq!(read_all(&store, b"c"), c);
}

fn pack_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join("packs"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

#[test]
fn retired_pack_whose_file_is_already_gone() {
    let dir = crash_child("compact", "compact_after_retire");
    // Find the files open deletes (the retired pack, and any orphans) by opening a copy.
    let probe = vfs_testkit::tempdir().unwrap();
    copy_dir(dir.path(), probe.path());
    drop(open(probe.path()));
    let after = pack_files(probe.path());
    let gone: Vec<String> = pack_files(dir.path())
        .into_iter()
        .filter(|f| !after.contains(f))
        .collect();
    assert!(!gone.is_empty(), "no retired pack to delete");
    // Delete them first, as if a previous open had removed the file but not the row.
    for f in &gone {
        std::fs::remove_file(dir.path().join("packs").join(f)).unwrap();
    }
    let store = check_after_crash(dir.path());
    assert_eq!(
        pack_files(dir.path()).len(),
        store.stats().unwrap().packs.len()
    );
}

#[test]
fn torn_tail_of_a_pack_sealed_after_a_crash() {
    let dir = crash_child("write", "write_after_append");
    // Tear the tail of the newest pack: a record header followed by only part of its payload.
    let newest = dir
        .path()
        .join("packs")
        .join(pack_files(dir.path()).last().unwrap());
    let bytes = std::fs::read(&newest).unwrap();
    let mut torn = bytes[..40].to_vec();
    torn.extend_from_slice(&[0xab; 10]);
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&newest)
        .unwrap();
    std::io::Write::write_all(&mut f, &torn).unwrap();
    drop(f);

    // The open is unclean, so the pack is sealed rather than resumed.
    let store = check_after_crash(dir.path());
    let report = store
        .compact(CompactOptions {
            min_garbage_ratio: 0.0,
            max_bytes: u64::MAX,
        })
        .unwrap();
    assert!(report.packs_compacted >= 1);
    assert!(!newest.exists(), "the torn pack was not compacted away");
    drop(store);
    check_after_crash(dir.path());
}
