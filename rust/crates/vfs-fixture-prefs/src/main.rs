//! The profile-API (INI) fixture: reads — and optionally writes — a key in a
//! file under a managed root through `GetPrivateProfileStringW` /
//! `WritePrivateProfileStringW`, the way Skyrim loads `SkyrimPrefs.ini`.
//!
//! ## Why this exists as its own fixture
//!
//! `vfs-fixture-read` reads with `std::fs::read`, which is
//! `NtCreateFile` → `NtReadFile` → `NtClose` and nothing else. The Windows
//! profile APIs take a different route through the same file:
//!
//! ```text
//! NtOpenFile → NtLockFile → NtQueryInformationFile → NtReadFile → NtUnlockFile → NtClose
//! ```
//!
//! `NtLockFile` had no detour in `vfs-shim`, so the real kernel got handed a
//! synthetic handle and answered `STATUS_INVALID_HANDLE`; the profile API then
//! abandoned the whole call and handed its caller back the *default* string it
//! was passed. The game therefore received no INI data at all — not stale data,
//! not real-disk data, just its own defaults. `std::fs::read` never locks, so
//! no existing fixture could see it. See
//! `.superpowers/sdd/2026-08-14-stage2a-ii-gate4-writes/prefs-read-investigation.md`.
//!
//! ## What it emits
//!
//! One tab-separated line per operation, to the file named by
//! `VFS_FIXTURE_INI_OUT` (created/truncated), or stdout if that is unset:
//!
//! ```text
//! read<TAB><value><TAB><lasterror>
//! write<TAB>ok|fail<TAB><lasterror>       (write mode only)
//! getfiletype<TAB>ok|fail<TAB><lasterror>
//! setfilepointer<TAB>ok|fail<TAB><lasterror>
//! lockfile<TAB>ok|fail<TAB><lasterror>
//! unlockfile<TAB>ok|fail<TAB><lasterror>
//! flushbuffers<TAB>ok|fail<TAB><lasterror>
//! lockfileex<TAB>ok|fail<TAB><lasterror>
//! {lock,unlock,flushbuffers}file-{closed,invalid}<TAB>ok|fail<TAB><lasterror>
//! ```
//!
//! The five lines after the reads are the investigation's "primitive ladder":
//! each Win32 call that a synthetic handle must survive, made one at a time
//! against the same handle, so a failure names the single NT call responsible
//! instead of surfacing as "the profile API returned the default". They are
//! what gives `NtUnlockFile` and `NtFlushBuffersFile` — which the profile
//! read path does not reach far enough to exercise on its own — an assertion
//! each.
//!
//! The harness — not this fixture — decides what the value should be. That
//! split is deliberate: the three failure modes this test has to separate
//! ("the director served it", "real disk served it", "nothing served it, the
//! API returned my default") are all *successful* API calls returning
//! different strings, so the fixture's exit code cannot carry the answer. It
//! reports what it saw and exits 0; the harness asserts which of the three it
//! is, and can name the wrong one in its failure message.
//!
//! ## Environment
//!
//! - `VFS_FIXTURE_INI_PATH` (required) — the INI file, under a managed root.
//! - `VFS_FIXTURE_INI_OUT` — output file. Must be **outside** every managed
//!   root, or writing the results would itself be part of what is under test.
//! - `VFS_FIXTURE_INI_WRITE` — when set, write this value to the key first
//!   (`WritePrivateProfileStringW`), then read it back.
//! - `VFS_FIXTURE_INI_SECTION` / `VFS_FIXTURE_INI_KEY` — override the
//!   section/key (default `[Display] sTest`).

#![allow(non_snake_case)]

use std::process::exit;

/// The default handed to `GetPrivateProfileStringW`. Receiving it back is the
/// exact symptom of the `NtLockFile` bug, so the harness matches on it by
/// name — it must not collide with any content either side of the test writes.
const DEFAULT_SENTINEL: &str = "MISSING";

#[link(name = "kernel32")]
extern "system" {
    fn GetPrivateProfileStringW(
        lpAppName: *const u16,
        lpKeyName: *const u16,
        lpDefault: *const u16,
        lpReturnedString: *mut u16,
        nSize: u32,
        lpFileName: *const u16,
    ) -> u32;

    fn WritePrivateProfileStringW(
        lpAppName: *const u16,
        lpKeyName: *const u16,
        lpString: *const u16,
        lpFileName: *const u16,
    ) -> i32;

    fn GetLastError() -> u32;
    fn SetLastError(dwErrCode: u32);

    fn CreateFileW(
        lpFileName: *const u16,
        dwDesiredAccess: u32,
        dwShareMode: u32,
        lpSecurityAttributes: *mut core::ffi::c_void,
        dwCreationDisposition: u32,
        dwFlagsAndAttributes: u32,
        hTemplateFile: *mut core::ffi::c_void,
    ) -> *mut core::ffi::c_void;
    fn CloseHandle(hObject: *mut core::ffi::c_void) -> i32;
    fn GetFileType(hFile: *mut core::ffi::c_void) -> u32;
    fn SetFilePointer(
        hFile: *mut core::ffi::c_void,
        lDistanceToMove: i32,
        lpDistanceToMoveHigh: *mut i32,
        dwMoveMethod: u32,
    ) -> u32;
    fn LockFile(
        hFile: *mut core::ffi::c_void,
        dwFileOffsetLow: u32,
        dwFileOffsetHigh: u32,
        nNumberOfBytesToLockLow: u32,
        nNumberOfBytesToLockHigh: u32,
    ) -> i32;
    fn UnlockFile(
        hFile: *mut core::ffi::c_void,
        dwFileOffsetLow: u32,
        dwFileOffsetHigh: u32,
        nNumberOfBytesToUnlockLow: u32,
        nNumberOfBytesToUnlockHigh: u32,
    ) -> i32;
    fn FlushFileBuffers(hFile: *mut core::ffi::c_void) -> i32;
    fn LockFileEx(
        hFile: *mut core::ffi::c_void,
        dwFlags: u32,
        dwReserved: u32,
        nNumberOfBytesToLockLow: u32,
        nNumberOfBytesToLockHigh: u32,
        lpOverlapped: *mut Overlapped,
    ) -> i32;
    fn CreateEventW(
        lpEventAttributes: *mut core::ffi::c_void,
        bManualReset: i32,
        bInitialState: i32,
        lpName: *const u16,
    ) -> *mut core::ffi::c_void;
}

/// Layout-compatible with Win32 `OVERLAPPED`. Only `hEvent` matters here: it
/// is what `LockFileEx` hands `NtLockFile` as its `Event`, which is the
/// completion shape the shim classifies (and the one it must signal).
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    h_event: *mut core::ffi::c_void,
}

const GENERIC_READ: u32 = 0x8000_0000;
const FILE_SHARE_ALL: u32 = 0x7;
const OPEN_EXISTING: u32 = 3;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILE_TYPE_UNKNOWN: u32 = 0;
const FILE_BEGIN: u32 = 0;
const INVALID_SET_FILE_POINTER: u32 = 0xFFFF_FFFF;
/// The byte range the lock calls use. A whole-file range is what a real INI
/// reader takes; the exact numbers do not matter to a synthetic handle, but a
/// nonzero length does — a zero-length lock is a degenerate case Windows can
/// answer without ever reaching the file object.
const LOCK_LEN: u32 = 64 * 1024;
const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x2;
const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x1;
const INVALID_HANDLE: *mut core::ffi::c_void = -1isize as *mut core::ffi::c_void;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// `GetPrivateProfileStringW(section, key, DEFAULT_SENTINEL, .., path)`.
/// Returns the string it produced and the `GetLastError` immediately after.
///
/// `SetLastError(0)` first: the API leaves the previous thread error in place
/// on a *successful* call, so without clearing it the reported code would be
/// whatever unrelated call ran before this one. The `6`
/// (`ERROR_INVALID_HANDLE`) this test hunts for is only meaningful because
/// nothing else could have set it.
fn read_key(path: &str, section: &str, key: &str) -> (String, u32) {
    let mut buf = vec![0u16; 512];
    // SAFETY: FFI. All three name pointers are NUL-terminated UTF-16 buffers
    // that outlive the call; `buf` is valid for `buf.len()` u16s, which is
    // what `nSize` says.
    unsafe {
        SetLastError(0);
        GetPrivateProfileStringW(
            wide(section).as_ptr(),
            wide(key).as_ptr(),
            wide(DEFAULT_SENTINEL).as_ptr(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            wide(path).as_ptr(),
        );
        (wide_to_string(&buf), GetLastError())
    }
}

fn write_key(path: &str, section: &str, key: &str, value: &str) -> (bool, u32) {
    // SAFETY: FFI. Every pointer is a NUL-terminated UTF-16 buffer that
    // outlives the call.
    unsafe {
        SetLastError(0);
        let ok = WritePrivateProfileStringW(
            wide(section).as_ptr(),
            wide(key).as_ptr(),
            wide(value).as_ptr(),
            wide(path).as_ptr(),
        );
        (ok != 0, GetLastError())
    }
}

/// Run the primitive ladder against one handle on `path`, appending a line per
/// call to `out`. Each call clears the thread error first, for the same reason
/// [`read_key`] does.
///
/// Every step reports rather than aborting the ladder: "the lock failed and
/// everything after it was never attempted" is exactly the shape of the bug
/// this fixture exists for, and a harness needs to see each call's own answer
/// to say which one broke.
fn primitive_ladder(path: &str, out: &mut String) {
    let wide_path = wide(path);
    // SAFETY: FFI. `wide_path` is a NUL-terminated UTF-16 buffer that outlives
    // the call; the two handle-shaped arguments are null, which this
    // disposition allows.
    let handle = unsafe {
        SetLastError(0);
        CreateFileW(
            wide_path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_ALL,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == (-1isize as *mut core::ffi::c_void) {
        // SAFETY: FFI, no arguments.
        let err = unsafe { GetLastError() };
        out.push_str(&format!("open\tfail\t{err}\n"));
        return;
    }
    out.push_str("open\tok\t0\n");

    // SAFETY: FFI. `handle` is the open handle from above, live until the
    // `CloseHandle` at the end of this function; `SetFilePointer`'s high-word
    // out-pointer is null, which the API allows for a 32-bit move.
    unsafe {
        SetLastError(0);
        let t = GetFileType(handle);
        let err = GetLastError();
        out.push_str(&format!(
            "getfiletype\t{}\t{err}\n",
            if t == FILE_TYPE_UNKNOWN { "fail" } else { "ok" }
        ));

        SetLastError(0);
        let pos = SetFilePointer(handle, 0, std::ptr::null_mut(), FILE_BEGIN);
        let err = GetLastError();
        out.push_str(&format!(
            "setfilepointer\t{}\t{err}\n",
            if pos == INVALID_SET_FILE_POINTER && err != 0 { "fail" } else { "ok" }
        ));

        SetLastError(0);
        let ok = LockFile(handle, 0, 0, LOCK_LEN, 0);
        let err = GetLastError();
        out.push_str(&format!("lockfile\t{}\t{err}\n", if ok != 0 { "ok" } else { "fail" }));

        SetLastError(0);
        let ok = UnlockFile(handle, 0, 0, LOCK_LEN, 0);
        let err = GetLastError();
        out.push_str(&format!("unlockfile\t{}\t{err}\n", if ok != 0 { "ok" } else { "fail" }));

        SetLastError(0);
        let ok = FlushFileBuffers(handle);
        let err = GetLastError();
        out.push_str(&format!("flushbuffers\t{}\t{err}\n", if ok != 0 { "ok" } else { "fail" }));

        // An *asynchronous-shaped* lock: `LockFileEx` hands the OVERLAPPED's
        // event down to `NtLockFile` as its `Event`. This is the caller shape
        // that can hang on a synthetic handle if the completion is never
        // delivered, so the shim has to both signal the event and classify the
        // call — see `hook::lock_hook`. A plain `LockFile` cannot exercise it:
        // it passes a null event and reads as an ordinary synchronous grant.
        let event = CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null());
        let mut ov = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            h_event: event,
        };
        SetLastError(0);
        let ok = LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            LOCK_LEN,
            0,
            &mut ov,
        );
        let err = GetLastError();
        out.push_str(&format!("lockfileex\t{}\t{err}\n", if ok != 0 { "ok" } else { "fail" }));
        if ok != 0 {
            SetLastError(0);
            UnlockFile(handle, 0, 0, LOCK_LEN, 0);
        }
        if !event.is_null() {
            CloseHandle(event);
        }

        CloseHandle(handle);

        // ── the same three calls on handles that must NOT be answered ──
        //
        // A synthetic handle is tagged by one bit, and `INVALID_HANDLE_VALUE`
        // (-1) has every bit set — so a shim that answers on the tag alone
        // reports a lock successfully taken on a handle nobody owns, and the
        // same for a handle that was closed a moment ago. Both must fail, and
        // they are separate vectors because a stale handle and a never-valid
        // one reach the check by different routes.
        for (label, h) in [("closed", handle), ("invalid", INVALID_HANDLE)] {
            SetLastError(0);
            let ok = LockFile(h, 0, 0, LOCK_LEN, 0);
            let err = GetLastError();
            out.push_str(&format!(
                "lockfile-{label}\t{}\t{err}\n",
                if ok != 0 { "ok" } else { "fail" }
            ));
            SetLastError(0);
            let ok = UnlockFile(h, 0, 0, LOCK_LEN, 0);
            let err = GetLastError();
            out.push_str(&format!(
                "unlockfile-{label}\t{}\t{err}\n",
                if ok != 0 { "ok" } else { "fail" }
            ));
            SetLastError(0);
            let ok = FlushFileBuffers(h);
            let err = GetLastError();
            out.push_str(&format!(
                "flushbuffers-{label}\t{}\t{err}\n",
                if ok != 0 { "ok" } else { "fail" }
            ));
        }
    }
}

fn main() {
    let path = std::env::var("VFS_FIXTURE_INI_PATH").unwrap_or_else(|_| {
        eprintln!("VFS_FIXTURE_INI_PATH unset");
        exit(2);
    });
    let section = std::env::var("VFS_FIXTURE_INI_SECTION").unwrap_or_else(|_| "Display".into());
    let key = std::env::var("VFS_FIXTURE_INI_KEY").unwrap_or_else(|_| "sTest".into());

    let mut out = String::new();

    if let Ok(value) = std::env::var("VFS_FIXTURE_INI_WRITE") {
        let (ok, err) = write_key(&path, &section, &key, &value);
        out.push_str(&format!("write\t{}\t{err}\n", if ok { "ok" } else { "fail" }));
    }

    let (value, err) = read_key(&path, &section, &key);
    out.push_str(&format!("read\t{value}\t{err}\n"));

    // A second read of the same key, after the first. Windows keeps a
    // process-wide INI cache, so this is the call that would come back from
    // the cache rather than from the file if one were in play — the harness
    // asserts both lines agree, which is what keeps "the director served it"
    // from being satisfied by a single lucky read.
    let (value2, err2) = read_key(&path, &section, &key);
    out.push_str(&format!("read\t{value2}\t{err2}\n"));

    primitive_ladder(&path, &mut out);

    match std::env::var("VFS_FIXTURE_INI_OUT") {
        Ok(dest) => {
            if let Err(e) = std::fs::write(&dest, out.as_bytes()) {
                eprintln!("FIXTURE FAIL: write results to {dest}: {e}");
                exit(2);
            }
        }
        Err(_) => print!("{out}"),
    }
    outlive_one_stats_tick();
    exit(0);
}

/// Stay alive long enough for the shim's hook-stats reporter to tick at least
/// once after the last call above.
///
/// The reporter is a periodic sample (`VFS_SHIM_STATS_LOG`), not an exit dump,
/// and **nothing flushes it at process exit** — `vfs_shim::hookstats::banner`
/// records why an exit flush was tried, measured, and removed. This whole run
/// is a few milliseconds, so without this wait the report can simply never
/// tick again after the calls a harness is reading it for, and their absence
/// would look identical to their never having happened.
///
/// Copied in shape (and in its 20ms floor) from `vfs-fixture-escape`'s
/// end-of-run wait: Windows' default timer granularity is ~15.6ms, so
/// `interval * 2` alone — 10ms at the 5ms interval the e2e tests configure —
/// does not reliably guarantee even one tick.
fn outlive_one_stats_tick() {
    if std::env::var_os("VFS_SHIM_STATS_LOG").is_none() {
        return;
    }
    let interval_ms: u64 = std::env::var("VFS_SHIM_STATS_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(250);
    let wait = std::time::Duration::from_millis(interval_ms.saturating_mul(2))
        .max(std::time::Duration::from_millis(20));
    std::thread::sleep(wait);
}
