//! `NtQueryObject` answers about events, mutexes, sections, registry keys and
//! threads — not just files. A handle the shim knows nothing about must come
//! back with the host's own answer, byte for byte.
//!
//! This is the guard rail on the identity spoof in
//! `identity_objectname.rs`: a hook that invents a name for an untracked
//! handle breaks unrelated Windows APIs in ways a file-focused suite never
//! catches, and it does so silently — the same shape as the `NtLockFile` gap
//! that broke all of Skyrim's INI loading undetected.
//!
//! Its own test binary because `install` is one-shot per process
//! (`ENGINE.set` returns `AlreadyInstalled` on a second call) and patches
//! process-global ntdll trampolines. Every hook test in this crate is a
//! single-test binary for that reason.
#![cfg(windows)]

use vfs_shim::{install, Engine};
use windows_sys::Win32::Foundation::HANDLE;

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryObject(h: isize, class: i32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
}
const OBJECT_NAME_INFORMATION: i32 = 1;

fn object_name(h: HANDLE) -> String {
    let mut buf = vec![0u8; 4096];
    let mut ret = 0u32;
    let st = unsafe {
        NtQueryObject(
            h as isize,
            OBJECT_NAME_INFORMATION,
            buf.as_mut_ptr(),
            buf.len() as u32,
            &mut ret,
        )
    };
    assert_eq!(st, 0, "NtQueryObject failed: 0x{st:08x}");
    let len = u16::from_le_bytes([buf[0], buf[1]]) as usize;
    // `as_chunks` rather than `chunks_exact`: clippy's
    // `chunks_exact_to_as_chunks` is denied workspace-wide.
    let chars: Vec<u16> = buf[16..16 + len]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    String::from_utf16_lossy(&chars)
}

#[test]
fn an_untracked_handle_is_untouched() {
    // NtQueryObject answers about events, mutexes and keys too. A handle the
    // shim knows nothing about must pass through with the host's own answer, or
    // the hook breaks unrelated Windows APIs.
    let pid = std::process::id();
    let plain_dir = std::env::temp_dir().join(format!("vfs-objname-plain-{pid}"));
    std::fs::create_dir_all(&plain_dir).unwrap();
    let plain = plain_dir.join("ordinary.txt");
    std::fs::write(&plain, b"x").unwrap();

    // Capture the answer with NO shim installed.
    use std::os::windows::io::AsRawHandle;
    let before = {
        let f = std::fs::File::open(&plain).unwrap();
        object_name(f.as_raw_handle() as HANDLE)
    };

    let root = std::env::temp_dir().join(format!("vfs-objname-plain-root-{pid}"));
    std::fs::create_dir_all(&root).unwrap();
    let snapshot = {
        use vfs_core::{build, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    let after = {
        let f = std::fs::File::open(&plain).unwrap();
        object_name(f.as_raw_handle() as HANDLE)
    };
    assert_eq!(before, after, "an untracked handle must be answered unchanged");

    // A file handle is the easy case: the hook's own table lookup misses and it
    // trampolines. The case that actually breaks unrelated Windows APIs is a
    // handle that is not a file at all, where inventing a name would be
    // undiagnosable -- so ask a named event, which lives in the object manager
    // namespace and has nothing to do with any filesystem.
    let evt_name: Vec<u16> = format!("vfs-objname-probe-{pid}\0").encode_utf16().collect();
    let evt = unsafe {
        windows_sys::Win32::System::Threading::CreateEventW(
            std::ptr::null(),
            1,
            0,
            evt_name.as_ptr(),
        )
    };
    assert!(!evt.is_null(), "could not create the probe event");
    let evt_reported = object_name(evt);
    unsafe { windows_sys::Win32::Foundation::CloseHandle(evt) };
    assert!(
        evt_reported.ends_with(&format!("vfs-objname-probe-{pid}")),
        "a non-file object must keep its own name: {evt_reported}"
    );
    assert!(
        !evt_reported.contains("vfs-objname-plain"),
        "the shim invented a filesystem name for an event: {evt_reported}"
    );
}
