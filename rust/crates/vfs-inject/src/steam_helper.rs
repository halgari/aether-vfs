//! Proton's Steam helper (`C:\windows\system32\steam.exe`), started beside
//! the target.
//!
//! `SteamAPI_IsSteamRunning` in a game's `steam_api64.dll` asks one thing:
//! is the process named by `HKCU\Software\Valve\Steam\ActiveProcess\pid`
//! alive. Under Proton that process is the helper. Started with `SteamGameId`
//! set and a program to run, it first writes its own pid to that key, then
//! sets up the rest (`SteamPath`, `lsteamclient`'s `SteamClientDll*` values
//! and its connection to the client, the prefix's `libraryfolders.vdf`), runs
//! the program, and makes itself a Wine system process: from then on it
//! stays for as long as the prefix runs any ordinary process and exits with
//! the last one.
//!
//! Proton makes the helper the game's parent (`steam.exe game.exe …`). Here
//! it is started beside the target instead, with a program of its own that
//! returns at once, because as a parent it gives its child no standard
//! handles and returns 0 whatever the child returned, and the injector's
//! caller reads both. The helper needs *a* program: given none, or one that
//! starts with `-` or `steam://`, it takes its command line for a Steam
//! command and hands it to the host's `steam` ([`check_helper_command`]).
//!
//! The wait ends when the pid is written, which is the helper's first step.
//! A helper that never gets that far is killed ([`start_steam_helper`]): it
//! is not yet a system process, so left alone it would keep the prefix, and
//! so the launch, alive after the game exits. One that hangs *after* writing
//! its pid (in `lsteamclient`, against a wedged client) is the remaining
//! case nothing here catches; stopping the launch (`wineserver -k`) ends it.

#![allow(unsafe_code)]

use std::os::windows::ffi::OsStrExt;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::Registry::{
    RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_DWORD, RRF_RT_REG_DWORD,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, TerminateProcess, WaitForSingleObject, PROCESS_INFORMATION, STARTUPINFOW,
};

/// The key (under `HKEY_CURRENT_USER`) the helper publishes itself under.
/// [`start_steam_helper`] takes the key as a parameter so tests can use one
/// of their own.
pub const ACTIVE_PROCESS_KEY: &str = r"Software\Valve\Steam\ActiveProcess";
/// The value holding the pid.
const ACTIVE_PROCESS_PID: &str = "pid";

/// A helper that has published itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SteamHelper {
    /// The helper's process id, as the key's `pid` now holds it.
    pub pid: u32,
    /// How long it took to publish itself.
    pub waited: Duration,
}

/// Why there is no helper to rely on. In every case the helper is no longer
/// running and the key's `pid` is 0, so the target sees no Steam client
/// rather than a stale or dying one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamHelperError {
    /// The command could not be started at all.
    NotStarted(String),
    /// The helper exited before publishing itself (or right after).
    ExitedEarly { pid: u32 },
    /// The helper did not publish itself within the bound, and was killed.
    TimedOut { pid: u32, after: Duration },
}

impl std::fmt::Display for SteamHelperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SteamHelperError::NotStarted(e) => write!(f, "it could not be started: {e}"),
            SteamHelperError::ExitedEarly { pid } => {
                write!(f, "it (process {pid}) exited before publishing itself")
            }
            SteamHelperError::TimedOut { pid, after } => write!(
                f,
                "it (process {pid}) did not get through start-up within {} s and was stopped",
                after.as_secs_f32()
            ),
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Whether this process runs under Wine: `ntdll` exports `wine_get_version`.
/// Under native Windows the key is the real Steam client's, and nothing here
/// may touch it.
pub fn running_under_wine() -> bool {
    // SAFETY: plain lookups on a module every process has mapped; the names
    // are NUL-terminated.
    unsafe {
        let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
        !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
    }
}

/// Refuses a helper command line that would make Proton's helper forward
/// itself to the host's `steam` instead of setting up: one with no program
/// after the helper, or whose program starts with `-` or `steam://`.
pub fn check_helper_command(command: &str) -> Result<(), String> {
    let mut words = command.split_whitespace();
    match (words.next(), words.next()) {
        (Some(_), Some(program))
            if !program.starts_with('-')
                && !program
                    .get(..8)
                    .is_some_and(|p| p.eq_ignore_ascii_case("steam://")) =>
        {
            Ok(())
        }
        _ => Err(format!(
            "refused helper command line {command:?}: it needs a program to run, or the helper \
             hands it to the host's steam"
        )),
    }
}

/// `HKCU\<key>\pid`, if it is set.
pub fn active_process_pid(key: &str) -> Option<u32> {
    let key = wide(key);
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

/// Sets `HKCU\<key>\pid` to 0 where it holds anything else; creates nothing.
///
/// A stale value is what makes `SteamAPI_IsSteamRunning` true by
/// coincidence: Wine numbers processes the same way in every run of a
/// prefix, so the pid an earlier helper left is often a live process of this
/// run. It would also end [`start_steam_helper`]'s wait before the new
/// helper had written anything.
pub fn clear_active_process_pid(key: &str) {
    if matches!(active_process_pid(key), None | Some(0)) {
        return;
    }
    let key = wide(key);
    let value = wide(ACTIVE_PROCESS_PID);
    let zero = 0u32;
    // SAFETY: both strings are NUL-terminated and outlive the call; the data
    // is the four bytes of `zero`. A failure leaves the old value.
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

/// Clears `HKCU\<key>\pid`, runs the command line `command`, and waits, for
/// at most `timeout`, until the process it starts has written its own pid
/// there. The helper is left running: it exits by itself when the prefix's
/// last ordinary process does.
///
/// On `Err` the helper is gone and the pid is 0 again: one that timed out is
/// killed, because a helper still in start-up is an ordinary process that
/// would keep the prefix — and the launch — alive after the game exits.
pub fn start_steam_helper(
    command: &str,
    key: &str,
    timeout: Duration,
) -> Result<SteamHelper, SteamHelperError> {
    clear_active_process_pid(key);
    let mut cmdline = wide(command);
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
        return Err(SteamHelperError::NotStarted(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    // SAFETY: both handles came from the successful `CreateProcessW` above
    // and are closed exactly once.
    unsafe { CloseHandle(pi.hThread) };
    let pid = pi.dwProcessId;
    let started = Instant::now();
    let outcome = loop {
        if active_process_pid(key) == Some(pid) {
            break Ok(SteamHelper {
                pid,
                waited: started.elapsed(),
            });
        }
        // SAFETY: `pi.hProcess` is the live process handle from above.
        if unsafe { WaitForSingleObject(pi.hProcess, 0) } == 0 {
            break Err(SteamHelperError::ExitedEarly { pid });
        }
        if started.elapsed() >= timeout {
            // SAFETY: the handle from `CreateProcessW` carries every access
            // right, terminate and synchronize included.
            unsafe {
                TerminateProcess(pi.hProcess, 1);
                WaitForSingleObject(pi.hProcess, 5_000);
            }
            break Err(SteamHelperError::TimedOut {
                pid,
                after: timeout,
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if outcome.is_err() {
        // A helper that wrote its pid and then died, or that was killed
        // mid-write, must not leave a pid that names a reused process.
        clear_active_process_pid(key);
    }
    // SAFETY: as above.
    unsafe { CloseHandle(pi.hProcess) };
    outcome
}
