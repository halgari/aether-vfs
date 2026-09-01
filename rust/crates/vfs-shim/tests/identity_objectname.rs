//! A redirected handle must not leak its backing path through NtQueryObject.
//!
//! `GetFinalPathNameByHandleW` takes different routes on different hosts —
//! `NtQueryInformationFile` on Windows, `NtQueryObject` on Wine — so a shim that
//! only spoofs the first answers correctly on one host and leaks on the other.
//!
//! Single-test binary: `install` is one-shot per process (`ENGINE.set` returns
//! `AlreadyInstalled` on a second call) and patches process-global ntdll
//! trampolines, so the untracked-handle half of this contract lives in
//! `identity_objectname_untracked.rs` rather than beside this test.
#![cfg(windows)]

use vfs_shim::{install, Engine};
use windows_sys::Win32::Foundation::HANDLE;

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryObject(h: isize, class: i32, info: *mut u8, len: u32, ret: *mut u32) -> i32;
}
const OBJECT_NAME_INFORMATION: i32 = 1;
/// Measured on both hosts: the buffer cannot hold even the 16-byte header.
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004u32 as i32;
/// Measured on both hosts: the header fits, the name does not.
const STATUS_BUFFER_OVERFLOW: i32 = 0x8000_0005u32 as i32;

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
fn a_redirected_handle_reports_its_virtual_name_not_the_backing_one() {
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-objname-{pid}"));
    let backing_dir = std::env::temp_dir().join(format!("vfs-objname-backing-{pid}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backing_dir).unwrap();
    let backing = backing_dir.join("backing_blob.dat");
    std::fs::write(&backing, b"the-real-bytes").unwrap();
    let vfile = root.join("mod.esp");

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "mod.esp".into(),
                kind: EntryKind::File,
                source: backing.to_str().unwrap().into(),
                size: 14,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    use std::os::windows::io::AsRawHandle;
    let f = std::fs::File::open(&vfile).expect("open redirected virtual file");
    let name = object_name(f.as_raw_handle() as HANDLE).to_lowercase();

    assert!(name.contains("mod.esp"), "must report the VIRTUAL name: {name}");
    assert!(
        !name.contains("backing_blob"),
        "must NOT leak the backing name: {name}"
    );

    // The size-probe contract, measured on Windows 11 and Wine 11.0
    // (GE-Proton11-6) on 2026-09-01 and identical on both. A caller that
    // queries with a tiny buffer, allocates what `ReturnLength` asks for and
    // queries again either loops forever or fails outright unless the spoof
    // reproduces this — and the spoofed name is a different length from the
    // real one, so `ReturnLength` here has to describe the *spoofed* buffer.
    let h = f.as_raw_handle() as HANDLE;
    let mut required = 0u32;
    for (len, expect) in [
        (0u32, STATUS_INFO_LENGTH_MISMATCH),
        (8, STATUS_INFO_LENGTH_MISMATCH),
        (16, STATUS_BUFFER_OVERFLOW),
    ] {
        let mut small = [0u8; 16];
        let mut ret = 0u32;
        let st = unsafe {
            NtQueryObject(
                h as isize,
                OBJECT_NAME_INFORMATION,
                small.as_mut_ptr(),
                len,
                &mut ret,
            )
        };
        assert_eq!(st, expect, "len={len}: expected 0x{expect:08x}, got 0x{st:08x}");
        assert_ne!(ret, 0, "len={len}: ReturnLength must carry the required size");
        if required == 0 {
            required = ret;
        }
        assert_eq!(ret, required, "len={len}: ReturnLength must be stable");
    }
    // One byte short of the requirement is still an overflow, not a success.
    let mut nearly = vec![0u8; required as usize];
    let mut ret = 0u32;
    let st = unsafe {
        NtQueryObject(
            h as isize,
            OBJECT_NAME_INFORMATION,
            nearly.as_mut_ptr(),
            required - 1,
            &mut ret,
        )
    };
    assert_eq!(st, STATUS_BUFFER_OVERFLOW, "required-1 must overflow: 0x{st:08x}");
    assert_eq!(ret, required);
    // And exactly the required size succeeds, with `Buffer` pointing 16 bytes
    // into the caller's own buffer -- what both hosts were measured to do.
    let mut exact = vec![0u8; required as usize];
    let st = unsafe {
        NtQueryObject(
            h as isize,
            OBJECT_NAME_INFORMATION,
            exact.as_mut_ptr(),
            required,
            &mut ret,
        )
    };
    assert_eq!(st, 0, "exactly `required` bytes must succeed: 0x{st:08x}");
    let namelen = u16::from_le_bytes([exact[0], exact[1]]) as usize;
    let maxlen = u16::from_le_bytes([exact[2], exact[3]]) as usize;
    let bufptr = usize::from_le_bytes(exact[8..16].try_into().unwrap());
    assert_eq!(maxlen, namelen + 2, "MaximumLength must include the NUL");
    assert_eq!(required as usize, 16 + namelen + 2);
    assert_eq!(
        bufptr.wrapping_sub(exact.as_ptr() as usize),
        16,
        "Buffer must point 16 bytes into the caller's own buffer"
    );
}
