//! Runs in its own process: path-based attribute queries reflect the VFS.
//!
//! `GetFileAttributesW` and `GetFileAttributesExW` reach `NtQueryAttributesFile` and
//! `NtQueryFullAttributesFile`, which under a managed root are answered by the director's
//! `getattr` alone (`hook/file_attr.rs::stat_by_path`).
//!
//! Before task C8 this binary installed the shim with no director, so the assertions had been
//! flipped to "nothing answers" (a virtual file invisible, a hidden real file visible). Through
//! the fake director it makes the original claims again: a served file and directory are
//! visible with the director's size and kind, a real file under the root that the director
//! does not serve is invisible (the sealed root's answer, where a snapshot tombstone used to
//! be), and a real file outside every root passes through.
use crate::fakedirector;

use fakedirector::{Fake, ReadStyle};
use std::ffi::c_void;
use vfs_shim::install;
use windows_sys::Win32::Storage::FileSystem::{
    GetFileAttributesExW, GetFileAttributesW, GetFileExInfoStandard, FILE_ATTRIBUTE_DIRECTORY,
    INVALID_FILE_ATTRIBUTES, WIN32_FILE_ATTRIBUTE_DATA,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[test]
fn attribute_queries_reflect_the_vfs() {
    isolate!();
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-shim-attrs-{pid}"));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(&root).unwrap();

    // A real file under the root that the director does not serve.
    let gone = root.join("gone.esp");
    std::fs::write(&gone, b"gone").unwrap();
    // A real file outside every root.
    let outside = base.join("outside.esp");
    std::fs::write(&outside, b"real").unwrap();

    // Virtual paths (absent on disk), served by the director.
    let vfile = root.join("mod.esp");
    let vdir = root.join("moddir");
    fakedirector::install(
        &root,
        Fake::new()
            .with("mod.esp", vec![0u8; 1234], ReadStyle::Whole)
            .with_dir("moddir"),
        0,
    );
    let _guard = install().expect("install");

    let a = unsafe { GetFileAttributesW(wide(vfile.to_str().unwrap()).as_ptr()) };
    assert_ne!(
        a, INVALID_FILE_ATTRIBUTES,
        "a served file must have attributes"
    );
    assert_eq!(
        a & FILE_ATTRIBUTE_DIRECTORY,
        0,
        "a served file is not a directory"
    );

    let d = unsafe { GetFileAttributesW(wide(vdir.to_str().unwrap()).as_ptr()) };
    assert_ne!(
        d, INVALID_FILE_ATTRIBUTES,
        "a served directory must have attributes"
    );
    assert_ne!(
        d & FILE_ATTRIBUTE_DIRECTORY,
        0,
        "a served directory must be a directory"
    );

    let g = unsafe { GetFileAttributesW(wide(gone.to_str().unwrap()).as_ptr()) };
    assert_eq!(
        g, INVALID_FILE_ATTRIBUTES,
        "a real file under the root that the director does not serve must be invisible"
    );

    let r = unsafe { GetFileAttributesW(wide(outside.to_str().unwrap()).as_ptr()) };
    assert_ne!(
        r, INVALID_FILE_ATTRIBUTES,
        "a real file outside every root passes through"
    );

    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileAttributesExW(
            wide(vfile.to_str().unwrap()).as_ptr(),
            GetFileExInfoStandard,
            &mut data as *mut _ as *mut c_void,
        )
    };
    assert_ne!(ok, 0, "GetFileAttributesExW must succeed for a served file");
    let size = (u64::from(data.nFileSizeHigh) << 32) | u64::from(data.nFileSizeLow);
    assert_eq!(
        size, 1234,
        "GetFileAttributesExW must report the director's size"
    );

    let _ = std::fs::remove_dir_all(&base);
}
