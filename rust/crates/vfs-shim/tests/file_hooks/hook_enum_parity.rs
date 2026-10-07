//! Runs in its own process: both directory-enumeration entry points show one view.
//!
//! ntdll exports `NtQueryDirectoryFile` and `NtQueryDirectoryFileEx`, and which
//! one a caller reaches is not our choice. If only one is hooked, the other
//! enumerates the real folder behind the mount — and a real folder that is
//! nearly empty (as a staged game tree is) returns a short listing rather than
//! an error. Nothing reports a problem; the caller simply concludes the
//! directory holds almost nothing.
//!
//! This is not hypothetical: the classic detour was once created but never
//! enabled, and a detour that is never enabled looks exactly like an API the
//! process never calls. A functional test per entry point is the only thing
//! that can tell those apart, so this compares the two directly.
//!
//! The listing is the director's (task C8 converted this from a no-director harness, where a
//! `Data` open had to be overlay-backed just to be openable and was then a plain OS listing of
//! the overlay directory, never the shim's own enumeration branch). Through the fake director
//! `Data` is a synthetic directory handle, both entry points reach `serve_dir_query`'s director
//! branch, and the expectations are the merged view's again: a served file the real directory
//! does not hold is listed, and a real file the director does not serve is not.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

use crate::fakedirector;
use crate::ntapi;
use fakedirector::{Fake, ReadStyle};
use ntapi::*;

#[test]
fn classic_and_ex_enumeration_agree() {
    isolate!();
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-shim-enumparity-{pid}"));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("gameroot");
    let data_dir = root.join("Data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // A real file the director does not serve: it must not be listed.
    std::fs::write(data_dir.join("hidden.esp"), b"h").unwrap();

    fakedirector::install(
        &root,
        Fake::new()
            .with_dir("data")
            .with("data/added.esm", vec![0u8; 7], ReadStyle::Whole)
            .with("data/real.txt", b"r".to_vec(), ReadStyle::Whole),
        0,
    );
    let _guard = vfs_shim::install().expect("install");

    // `read_dir` goes through NtQueryDirectoryFileEx.
    let mut via_ex: Vec<String> = std::fs::read_dir(&data_dir)
        .expect("read_dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();

    let dir = open_dir(&data_dir);
    assert!(!dir.is_null(), "could not open the directory");
    let mut via_classic: Vec<String> = nt_enum_classic(dir)
        .into_iter()
        .filter(|n| n != "." && n != "..")
        .collect();
    close(dir);

    via_ex.sort();
    via_classic.sort();
    via_classic.dedup();

    assert!(
        !via_classic.is_empty(),
        "the classic entry point returned nothing — is its detour enabled?"
    );
    assert_eq!(
        via_classic, via_ex,
        "the two enumeration entry points disagree; one of them is not virtualised"
    );

    // Spell out what the listing must contain, so a result that is merely
    // *consistently wrong* still fails.
    assert!(
        via_classic.iter().any(|n| n == "real.txt"),
        "real.txt missing: {via_classic:?}"
    );
    assert!(
        via_classic.iter().any(|n| n == "added.esm"),
        "a served file the real directory does not hold must be listed: {via_classic:?}"
    );
    assert!(
        !via_classic.iter().any(|n| n == "hidden.esp"),
        "a real file the director does not serve leaked into the listing: {via_classic:?}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

fn open_dir(path: &std::path::Path) -> *mut c_void {
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        CreateFileW(
            wide.as_ptr(),
            0x0010_0000 | 1, // SYNCHRONIZE | FILE_LIST_DIRECTORY
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            core::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            core::ptr::null_mut(),
        )
    }
}
