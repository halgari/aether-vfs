//! `WineLaunch::log_file`: where the `wine` child's stdout and stderr go.
//!
//! Against a fake GE runtime whose `wine` is a shell script that writes to
//! both streams, and whose background "child" writes after `wine` exits — the
//! way `wineserver` and the game outlive the `wine` that started them and keep
//! the descriptors they inherited.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vfs_proton::launch::{run, WineLaunch};

fn scratch(tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("vfs-launch-log-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A runtime directory that passes `verify_ge`, with a fake `wine`.
fn fake_runtime(dir: &Path) -> PathBuf {
    let rt = dir.join("GE-Proton99-1");
    let bin = rt.join("files").join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(rt.join("version"), "1 GE-Proton99-1\n").unwrap();
    let wine = bin.join("wine");
    std::fs::write(
        &wine,
        r#"#!/bin/sh
echo "fake wine stdout"
echo "err:seh:fake wine stderr" >&2
( sleep 0.2; echo "fake child after wine exited" ) &
exit 0
"#,
    )
    .unwrap();
    std::fs::set_permissions(&wine, std::fs::Permissions::from_mode(0o755)).unwrap();
    rt
}

fn launch(dir: &Path, log_file: Option<PathBuf>) -> WineLaunch {
    WineLaunch {
        runtime: fake_runtime(dir),
        prefix: dir.join("pfx"),
        injector: dir.join("vfs-injector.exe"),
        shim_dll: dir.join("vfs_shim_dll.dll"),
        payload_dll: dir.join("vfs_payload.dll"),
        target: r"C:\probe\target.exe".to_string(),
        config_file: dir.join("shim.cfg"),
        ready_file: dir.join("ready.flag"),
        ring_path: PathBuf::from(r"C:\probe\ring.bin"),
        ring_bytes: 33_751_040,
        arena_offset: 65_536,
        arena_len: 33_554_432,
        payload_cap: 1_048_576,
        virtual_dir: r"C:\probe\managed".to_string(),
        virtual_roots: vec![],
        args: vec![],
        extra_env: BTreeMap::new(),
        cwd: None,
        ready_timeout_secs: None,
        log_file,
        steam: vfs_proton::SteamSide::Untouched,
        notes: Vec::new(),
    }
}

fn wait_for_line(log: &Path, line: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = std::fs::read_to_string(log).unwrap_or_default();
        if s.contains(line) {
            return s;
        }
        assert!(
            Instant::now() < deadline,
            "{line:?} never reached {}: {s:?}",
            log.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn stdout_and_stderr_of_wine_and_its_children_go_to_the_log_file() {
    let dir = scratch("set");
    // Its parent directories do not exist yet: the launch creates them.
    let log = dir.join("logs").join("nested").join("wine.log");

    assert_eq!(run(&launch(&dir, Some(log.clone()))).unwrap(), 0);
    let s = wait_for_line(&log, "fake child after wine exited");
    assert!(s.contains("fake wine stdout\n"), "{s:?}");
    assert!(s.contains("err:seh:fake wine stderr\n"), "{s:?}");
}

#[test]
fn a_log_file_is_truncated_at_launch() {
    let dir = scratch("trunc");
    let log = dir.join("wine.log");
    std::fs::write(&log, "a previous run's output\n").unwrap();
    assert_eq!(run(&launch(&dir, Some(log.clone()))).unwrap(), 0);
    let s = wait_for_line(&log, "fake child after wine exited");
    assert!(!s.contains("previous run"), "{s:?}");
}

#[test]
fn a_launchs_notes_lead_the_log_file() {
    let dir = scratch("notes");
    let log = dir.join("wine.log");
    let mut l = launch(&dir, Some(log.clone()));
    l.notes = vec!["first note".to_string(), "second note".to_string()];
    assert_eq!(run(&l).unwrap(), 0);
    let s = wait_for_line(&log, "fake child after wine exited");
    assert!(s.starts_with("first note\nsecond note\n"), "{s:?}");
    assert!(
        s.contains("fake wine stdout") && s.contains("fake wine stderr"),
        "{s:?}"
    );
}

#[test]
fn an_unwritable_log_file_fails_the_launch_before_spawning() {
    let dir = scratch("bad");
    // A directory where the file should be: it cannot be created.
    let log = dir.join("wine.log");
    std::fs::create_dir_all(&log).unwrap();
    let e = run(&launch(&dir, Some(log.clone()))).unwrap_err();
    assert!(e.to_string().contains("wine.log"), "{e}");
}

#[test]
fn no_log_file_leaves_the_streams_inherited() {
    let dir = scratch("unset");
    assert_eq!(run(&launch(&dir, None)).unwrap(), 0);
    let stray: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".log"))
        .collect();
    assert!(stray.is_empty(), "{stray:?}");
}
