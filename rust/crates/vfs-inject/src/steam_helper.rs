//! Proton's Steam helper (`C:\windows\system32\steam.exe`), started beside
//! the target.
//!
//! `SteamAPI_IsSteamRunning` in a game's `steam_api64.dll` asks one thing:
//! is the process named by `HKCU\Software\Valve\Steam\ActiveProcess\pid`
//! alive. Under Proton that process is the helper. Started with `SteamGameId`
//! set, it writes its own pid and the `SteamClientDll` paths to that key
//! (through `lsteamclient`), then makes itself a Wine system process: it
//! stays for as long as the prefix runs any ordinary process and exits with
//! the last one.
//!
//! Proton makes the helper the game's parent (`steam.exe game.exe …`). Here
//! it is started beside the target instead, with a program of its own that
//! returns at once, because as a parent it gives its child no standard
//! handles and returns 0 whatever the child returned, and the injector's
//! caller reads both. The helper needs *a* program: given none, it takes its
//! command line for a Steam command and hands it to the host's `steam`.

#![allow(unsafe_code)]

use std::os::windows::ffi::OsStrExt;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Registry::{
    RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, RRF_RT_REG_DWORD,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, WaitForSingleObject, PROCESS_INFORMATION, STARTUPINFOW,
};

/// The key the helper publishes itself under, and the value holding its pid.
const ACTIVE_PROCESS_KEY: &str = r"Software\Valve\Steam\ActiveProcess";
const ACTIVE_PROCESS_PID: &str = "pid";
/// What makes the helper do its setup at all; without it the helper returns
/// at once, having written nothing.
const STEAM_GAME_ID: &str = "SteamGameId";

/// A helper that has published itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SteamHelper {
    /// The helper's process id, as `ActiveProcess\pid` now holds it.
    pub pid: u32,
    /// How long it took to publish itself.
    pub waited: Duration,
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// `HKCU\Software\Valve\Steam\ActiveProcess\pid`, if it is set.
fn active_process_pid() -> Option<u32> {
    let key = wide(ACTIVE_PROCESS_KEY);
    let value = wide(ACTIVE_PROCESS_PID);
    let mut pid = 0u32;
    let mut len = size_of::<u32>() as u32;
    // SAFETY: both strings are NUL-terminated and outlive the call; `pid` is
    // the `len` bytes the call may write.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            core::ptr::null_mut(),
            (&mut pid as *mut u32).cast(),
            &mut len,
        )
    };
    (rc == 0).then_some(pid)
}

/// Sets `ActiveProcess\pid` to 0 where it holds anything else. Wine numbers
/// processes the same way in every run of a prefix, so the value an earlier
/// helper left is often the very pid the next one gets, and the wait below
/// would end before the new helper had written anything.
fn clear_active_process_pid() {
    if matches!(active_process_pid(), None | Some(0)) {
        return;
    }
    let key = wide(ACTIVE_PROCESS_KEY);
    let value = wide(ACTIVE_PROCESS_PID);
    let zero = 0u32;
    // SAFETY: both strings are NUL-terminated and outlive the call; the data
    // is the four bytes of `zero`. A failure leaves the old value, which the
    // wait then tolerates as before.
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            REG_DWORD,
            (&zero as *const u32).cast(),
            size_of::<u32>() as u32,
        );
    }
}

/// Runs the command line `helper` and waits, for at most `timeout`, until the
/// process it starts has published itself as the running Steam client. The helper is left running: it exits
/// by itself when the prefix's last ordinary process does.
///
/// `Err` says why there is no helper to rely on — it could not be started,
/// it exited, or it published nothing in time (`lsteamclient` disabled or
/// missing). None of these stops a launch; the target then sees no running
/// Steam client, as it did before the helper existed.
pub fn start_steam_helper(helper: &str, timeout: Duration) -> Result<SteamHelper, String> {
    if std::env::var_os(STEAM_GAME_ID).is_none() {
        return Err(format!(
            "{STEAM_GAME_ID} is not set, so the helper would set nothing up"
        ));
    }
    clear_active_process_pid();
    let mut cmdline = wide(helper);
    // SAFETY: zero is a valid value of both structs; `cb` is set below.
    let (mut si, mut pi): (STARTUPINFOW, PROCESS_INFORMATION) =
        unsafe { (core::mem::zeroed(), core::mem::zeroed()) };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    // SAFETY: `cmdline` is a writable NUL-terminated buffer that outlives the
    // call, as `CreateProcessW` requires; the other pointers are null or
    // point at the two structs above.
    let created = unsafe {
        CreateProcessW(
            core::ptr::null(),
            cmdline.as_mut_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            0,
            0,
            core::ptr::null(),
            core::ptr::null(),
            &si,
            &mut pi,
        )
    };
    if created == 0 {
        return Err(format!(
            "could not start it: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: both handles came from the successful `CreateProcessW` above
    // and are closed exactly once.
    unsafe { CloseHandle(pi.hThread) };
    let started = Instant::now();
    let outcome = loop {
        if active_process_pid() == Some(pi.dwProcessId) {
            break Ok(SteamHelper {
                pid: pi.dwProcessId,
                waited: started.elapsed(),
            });
        }
        // SAFETY: `pi.hProcess` is the live process handle from above.
        if unsafe { WaitForSingleObject(pi.hProcess, 0) } == 0 {
            break Err("it exited before publishing itself".to_string());
        }
        if started.elapsed() >= timeout {
            break Err(format!(
                "it did not publish itself within {} s (is lsteamclient disabled?)",
                timeout.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // SAFETY: as above.
    unsafe { CloseHandle(pi.hProcess) };
    outcome
}
