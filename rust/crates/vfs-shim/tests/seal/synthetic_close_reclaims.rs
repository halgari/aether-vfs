//! Closing a director-served (synthetic) handle drops its record from the shim's handle table.
//!
//! The synthetic branch of `NtClose` used to return before the table was touched. Synthetic
//! handle values only increase, so nothing ever reused a key to clear it: every served open left
//! a record for the life of the process, and after 65,536 of them the `opened_as` budget was
//! gone for every handle. Witness: the table's size across many open/close pairs.

use crate::fakedirector;

use fakedirector::{Fake, ReadStyle};
use std::os::windows::ffi::OsStrExt;
use vfs_shim::{install, tracked_handle_count};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

fn open(path: &std::path::Path) -> windows_sys::Win32::Foundation::HANDLE {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            core::ptr::null(),
            OPEN_EXISTING,
            0,
            core::ptr::null_mut(),
        )
    }
}

#[test]
fn closing_synthetic_handles_does_not_grow_the_handle_table() {
    isolate!();
    let base = std::env::temp_dir().join(format!("vfs-synth-close-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(root.join("data")).unwrap();
    let fake = fakedirector::install(
        &root,
        Fake::new()
            .with_dir("data")
            .with("data/a.esp", b"served".to_vec(), ReadStyle::Whole),
        0,
    );
    let _ = &fake;
    let hooks = install().expect("install");
    let served = root.join("data").join("a.esp");

    // Warm-up: whatever lazy state the first open creates is not a leak.
    let h = open(&served);
    assert_ne!(h, INVALID_HANDLE_VALUE, "the served open failed");
    unsafe { CloseHandle(h) };

    let before = tracked_handle_count();
    for _ in 0..500 {
        let h = open(&served);
        assert_ne!(h, INVALID_HANDLE_VALUE, "the served open failed");
        unsafe { CloseHandle(h) };
    }
    let after = tracked_handle_count();
    drop(hooks);

    assert!(
        after <= before + 8,
        "500 open/close pairs of a served file grew the handle table from {before} to {after}: \
         a synthetic close leaves its record behind"
    );
    let _ = std::fs::remove_dir_all(&base);
}
