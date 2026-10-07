//! A child process the shim cannot inject is killed and its `CreateProcess`
//! fails; it is never resumed without the shim.
//!
//! The hooks are installed in this test's own process, so the process-creation
//! hook is live for the `cmd` the test spawns. The "shim DLL" it would inject
//! is this test executable (the hook injects the image it lives in), which is
//! not a loadable shim: whichever step gives out first (no payload found
//! beside it, or the child never reporting ready within the 2 s timeout set
//! here), the rule is the same, and the three assertions are the rule:
//!
//! 1. the spawn fails (with `ERROR_PROCESS_ABORTED`),
//! 2. no child process of this one is left, and
//! 3. the child never ran (it writes a marker file as its one act), and
//! 4. the refusal is logged for the launcher, as `<image> <reason>`, in the
//!    file `VFS_CHILD_REFUSED_LOG` names.
//!
//! The success half (a child that injects fine still runs, virtualised) needs
//! the real shim DLL and a director, so it is the Proton end-to-end test
//! `a_child_the_fixture_spawns_is_virtualised_too_under_proton`.
use crate::fakedirector::{self, Fake};

use std::os::windows::process::CommandExt;
use std::time::Duration;
use vfs_shim::{child_inject_refused_total, install};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_PROCESS_ABORTED, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

/// The processes whose parent is this one.
fn children() -> Vec<(u32, String)> {
    let me = std::process::id();
    let mut found = Vec::new();
    // SAFETY: a process snapshot walked with a correctly sized entry; the handle
    // is closed before returning.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return found;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            if e.th32ParentProcessID == me {
                let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                found.push((e.th32ProcessID, String::from_utf16_lossy(&e.szExeFile[..n])));
            }
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
    }
    found
}

#[test]
fn a_child_that_cannot_be_injected_is_killed_and_its_spawn_fails() {
    isolate!();
    let base = std::env::temp_dir().join(format!("vfs-shim-child-closed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let marker = base.join("child-ran.txt");

    fakedirector::install(&root, Fake::new().with_dir("."), 0);
    // A short allowance so a child that never reports ready fails the test fast;
    // the payload named here does not exist.
    std::env::set_var(vfs_env::READY_TIMEOUT_SECS, "2");
    std::env::set_var(vfs_env::PAYLOAD_PATH, base.join("no-such-payload.dll"));
    let refused_log = base.join("ready.flag.child-refused");
    std::env::set_var(vfs_env::CHILD_REFUSED_LOG, &refused_log);
    let _guard = install().expect("install");

    let refused_before = child_inject_refused_total();
    // The marker is written by the shell as the child's one act.
    let spawned = std::process::Command::new("cmd")
        .arg("/C")
        .raw_arg(format!("echo ran>\"{}\"", marker.display()))
        .spawn();

    let err = match spawned {
        Ok(mut c) => {
            let _ = c.kill();
            panic!("the spawn of an uninjectable child succeeded");
        }
        Err(e) => e,
    };
    assert_eq!(
        err.raw_os_error(),
        Some(ERROR_PROCESS_ABORTED as i32),
        "the spawn must fail with ERROR_PROCESS_ABORTED, got {err}"
    );
    assert_eq!(
        child_inject_refused_total(),
        refused_before + 1,
        "the refusal must be counted"
    );

    // The launcher can learn of it: one `<image> <reason>` line, whose reason
    // is one the counter has just counted.
    let log = std::fs::read_to_string(&refused_log).expect("the refusal must be logged");
    let line = log.lines().next().expect("a line");
    assert_eq!(log.lines().count(), 1, "{log:?}");
    let (image, reason) = line.rsplit_once(' ').expect("`<image> <reason>`");
    assert!(image.to_lowercase().contains("cmd"), "image {image:?} in {line:?}");
    assert!(
        vfs_shim::child_inject_refused_count(reason) >= 1,
        "reason {reason:?} in {line:?} is not one the counter knows"
    );

    // The hook waits for the kill before returning, but allow the process
    // table a moment.
    let mut left = children();
    for _ in 0..40 {
        if left.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        left = children();
    }
    assert!(left.is_empty(), "child processes still alive after the refused spawn: {left:?}");

    std::thread::sleep(Duration::from_millis(500));
    assert!(!marker.exists(), "the refused child ran: {marker:?} exists");
    let _ = std::fs::remove_dir_all(&base);
}
