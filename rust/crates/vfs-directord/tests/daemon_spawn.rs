//! An auto-spawned daemon that cannot start must say why, promptly.
//!
//! `vfs <command>` auto-spawns `vfs daemon` when none is running. That daemon
//! is detached, so its stderr goes to `<discovery>.daemon.log`; when it exits
//! before becoming ready, the client reports its status and that log instead
//! of polling for 15 s and saying only "daemon did not become ready".

use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn an_auto_spawned_daemon_that_cannot_open_its_storage_reports_why() {
    let t = tempfile::tempdir().unwrap();
    let storage_dir = t.path().join("storage");
    // Held by this process for the whole test: the spawned daemon's open
    // fails with the store's lock error.
    let held = vfs_embed::Storage::open(&storage_dir, vfs_embed::StorageConfig::default())
        .expect("hold the storage");
    let discovery = t.path().join("discovery.json");

    let start = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_vfs"))
        .args(["--discovery", discovery.to_str().unwrap(), "health"])
        .env("VFS_STORAGE_DIR", &storage_dir)
        .env("VFS_HOME", t.path().join("home"))
        .output()
        .expect("run vfs health");
    let took = start.elapsed();
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(!out.status.success(), "health must fail: {stderr}");
    assert!(
        took < Duration::from_secs(8),
        "the failure must not wait out the 15 s readiness timeout (took {took:?}): {stderr}"
    );
    assert!(
        stderr.contains("daemon exited") && stderr.contains("locked"),
        "the error must carry the daemon's own storage error: {stderr}"
    );
    let log = std::fs::read_to_string(t.path().join("discovery.json.daemon.log"))
        .expect("the daemon's stderr is kept beside the discovery file");
    assert!(log.contains("cannot open storage"), "{log}");
    drop(held);
}
