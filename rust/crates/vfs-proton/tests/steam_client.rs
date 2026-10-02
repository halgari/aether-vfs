//! `vfs_proton::steam`: whether a Steam client is running, from its pid file
//! and a process table.

use std::path::{Path, PathBuf};

use vfs_proton::steam::{not_running_note, running_client_in};

/// Scratch under Cargo's `CARGO_TARGET_TMPDIR`, not `/tmp`.
fn scratch(tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("vfs-proton-steam-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn proc_with(dir: &Path, pid: u32, comm: &str) -> PathBuf {
    let proc = dir.join("proc");
    std::fs::create_dir_all(proc.join(pid.to_string())).unwrap();
    std::fs::write(proc.join(pid.to_string()).join("comm"), format!("{comm}\n")).unwrap();
    proc
}

#[test]
fn a_pid_file_naming_a_live_steam_process_is_a_running_client() {
    let d = scratch("live");
    std::fs::write(d.join("steam.pid"), "4242\n").unwrap();
    let proc = proc_with(&d, 4242, "steam");
    assert_eq!(running_client_in(&d, &proc), Some(4242));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_stale_pid_file_is_not_a_running_client() {
    let d = scratch("stale");
    let proc = proc_with(&d, 1, "systemd");
    assert_eq!(running_client_in(&d, &proc), None, "no pid file");
    std::fs::write(d.join("steam.pid"), "4242").unwrap();
    assert_eq!(running_client_in(&d, &proc), None, "the process is gone");
    std::fs::write(d.join("steam.pid"), "1").unwrap();
    assert_eq!(
        running_client_in(&d, &proc),
        None,
        "the pid was reused by something else"
    );
    std::fs::write(d.join("steam.pid"), "not a pid").unwrap();
    assert_eq!(running_client_in(&d, &proc), None);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn the_note_is_one_line_naming_the_pid_file() {
    let n = not_running_note(Path::new("/home/u/.steam"));
    assert!(
        n.contains("/home/u/.steam/steam.pid") && n.contains("without Steam"),
        "{n}"
    );
    assert!(!n.contains('\n'));
}
