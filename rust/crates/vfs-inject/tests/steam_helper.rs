//! `start_steam_helper` against a stand-in helper (`vfs-fake-steam-helper`)
//! and a registry key of the test's own, never Steam's: the target must
//! still be startable after a bounded wait whatever the helper does, and a
//! failed helper must be gone afterwards with the pid cleared.
#![cfg(windows)]
#![allow(unsafe_code)]

use std::os::windows::ffi::OsStrExt;
use std::time::{Duration, Instant};

use vfs_inject::{
    active_process_pid, check_helper_command, clear_active_process_pid, start_steam_helper,
    SteamHelperError,
};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Registry::{
    RegDeleteTreeW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD,
};
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE,
};

const FAKE: &str = env!("CARGO_BIN_EXE_vfs-fake-steam-helper");

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// A key under `HKCU\Software\aether-vfs-test`, deleted on drop.
struct TestKey(String);

impl TestKey {
    fn new(tag: &str) -> Self {
        TestKey(format!(
            r"Software\aether-vfs-test\steam-helper-{}-{tag}\ActiveProcess",
            std::process::id()
        ))
    }

    fn set_pid(&self, pid: u32) {
        let (key, value) = (wide(&self.0), wide("pid"));
        // SAFETY: NUL-terminated strings and four bytes of data.
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                value.as_ptr(),
                REG_DWORD,
                (&pid as *const u32).cast(),
                4,
            );
        }
    }
}

impl Drop for TestKey {
    fn drop(&mut self) {
        let parent = self.0.trim_end_matches(r"\ActiveProcess");
        // SAFETY: a NUL-terminated key name.
        unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(parent).as_ptr()) };
    }
}

fn command(args: &str) -> String {
    format!("\"{FAKE}\" {args}")
}

/// Whether `pid` names a process that has not exited.
fn alive(pid: u32) -> bool {
    // SAFETY: plain calls; the handle is closed before returning.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        ok && code == 259
    }
}

fn kill(pid: u32) {
    // SAFETY: plain calls; the handle is closed before returning.
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if !h.is_null() {
            TerminateProcess(h, 0);
            CloseHandle(h);
        }
    }
}

#[test]
fn a_helper_that_publishes_its_pid_is_reported_and_left_running() {
    let key = TestKey::new("ok");
    let h = start_steam_helper(
        &command(&format!("publish {}", key.0)),
        &key.0,
        Duration::from_secs(20),
    )
    .expect("the stand-in publishes itself");
    assert_eq!(active_process_pid(&key.0), Some(h.pid));
    assert!(
        alive(h.pid),
        "a helper that published itself is left to Wine to end"
    );
    kill(h.pid);
}

#[test]
fn a_command_that_cannot_start_fails_at_once() {
    let key = TestKey::new("nostart");
    let t = Instant::now();
    let e = start_steam_helper(
        r"C:\no\such\steam-helper.exe x",
        &key.0,
        Duration::from_secs(20),
    )
    .unwrap_err();
    assert!(matches!(e, SteamHelperError::NotStarted(_)), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
}

#[test]
fn a_helper_that_exits_before_publishing_fails_at_once_and_leaves_no_pid() {
    let key = TestKey::new("exits");
    key.set_pid(4242);
    let t = Instant::now();
    let e = start_steam_helper(&command("exit"), &key.0, Duration::from_secs(20)).unwrap_err();
    assert!(matches!(e, SteamHelperError::ExitedEarly { .. }), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    assert_eq!(active_process_pid(&key.0), Some(0));
}

#[test]
fn a_helper_that_never_publishes_is_stopped_at_the_bound() {
    let key = TestKey::new("timeout");
    let t = Instant::now();
    let e = start_steam_helper(&command("sleep"), &key.0, Duration::from_millis(300)).unwrap_err();
    let elapsed = t.elapsed();
    let SteamHelperError::TimedOut { pid, .. } = e else {
        panic!("expected TimedOut, got {e:?}");
    };
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
    // Left running, it would keep a Wine prefix (and the launch) alive.
    assert!(!alive(pid), "the stuck helper must be gone");
    assert_ne!(active_process_pid(&key.0), Some(pid));
}

#[test]
fn a_stale_pid_is_cleared_and_a_missing_one_is_not_created() {
    let key = TestKey::new("stale");
    clear_active_process_pid(&key.0);
    assert_eq!(active_process_pid(&key.0), None, "nothing is created");
    key.set_pid(1234);
    clear_active_process_pid(&key.0);
    assert_eq!(active_process_pid(&key.0), Some(0));
}

#[test]
fn a_command_line_the_helper_would_forward_to_the_host_is_refused() {
    let ok = r"C:\windows\system32\steam.exe C:\windows\system32\rundll32.exe";
    assert!(check_helper_command(ok).is_ok());
    for bad in [
        r"C:\windows\system32\steam.exe",
        r"C:\windows\system32\steam.exe -silent",
        r"C:\windows\system32\steam.exe steam://rungameid/489830",
        r"C:\windows\system32\steam.exe STEAM://run/1",
        "",
    ] {
        assert!(check_helper_command(bad).is_err(), "{bad:?}");
    }
}
