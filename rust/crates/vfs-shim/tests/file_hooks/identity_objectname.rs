//! A virtual file's handle must answer `NtQueryObject` with its virtual name, and follow the
//! host's size-probe contract exactly.
//!
//! `GetFinalPathNameByHandleW` takes different routes on different hosts —
//! `NtQueryInformationFile` on Windows, `NtQueryObject` on Wine — so a shim that
//! only answers the first is right on one host and wrong on the other.
//!
//! The file is served by the (fake) director, so the handle is synthetic and the kernel has no
//! name for it at all: the shim's answer is the only one. Before task C8 this was a snapshot
//! redirect to a real backing file (a real handle whose name had to be replaced); the answer
//! goes through the same `emit_object_name`, so the size-probe half is unchanged.
//!
//! Runs in its own process: `install` patches process-global ntdll trampolines, so the
//! untracked-handle half of this contract lives in `identity_objectname_untracked.rs`.

use crate::fakedirector;

use fakedirector::{Fake, ReadStyle};
use vfs_shim::install;
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
    isolate!();
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-objname-{pid}"));
    std::fs::create_dir_all(&root).unwrap();
    let vfile = root.join("mod.esp");
    fakedirector::install(
        &root,
        Fake::new().with("mod.esp", b"the-real-bytes".to_vec(), ReadStyle::Whole),
        0,
    );
    let _guard = install().expect("install");

    use std::os::windows::io::AsRawHandle;
    let f = std::fs::File::open(&vfile).expect("open the virtual file");
    let name = object_name(f.as_raw_handle() as HANDLE).to_lowercase();

    assert!(
        name.ends_with(r"\mod.esp") && name.contains(&format!("vfs-objname-{pid}")),
        "must report the VIRTUAL name: {name}"
    );

    // The size-probe contract, measured on Windows 11 and Wine 11.0
    // (GE-Proton11-6) on 2026-09-01 and identical on both. A caller that
    // queries with a tiny buffer, allocates what `ReturnLength` asks for and
    // queries again either loops forever or fails outright unless the spoof
    // reproduces this, and `ReturnLength` has to describe the shim's own answer.
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
        assert_eq!(
            st, expect,
            "len={len}: expected 0x{expect:08x}, got 0x{st:08x}"
        );
        assert_ne!(
            ret, 0,
            "len={len}: ReturnLength must carry the required size"
        );
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
    assert_eq!(
        st, STATUS_BUFFER_OVERFLOW,
        "required-1 must overflow: 0x{st:08x}"
    );
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
