//! A file opened with `FILE_DELETE_ON_CLOSE` under a managed root is deleted at the director when
//! it is closed, and the real file under the root is untouched.
//!
//! **Found by task C8.** Wine's `DeleteFileW` (kernelbase) is not `NtSetInformationFile`: it is
//! an `NtCreateFile` with `FILE_DELETE_ON_CLOSE` followed by `NtClose`. The shim honoured neither
//! half for a synthetic handle (the director has no delete-on-close, and the close only released
//! the `fh`), so under Proton every Win32 delete of a director-served file reported success and
//! deleted nothing. `hook_write`'s delete section, which had never run before C8, is where it
//! showed. Windows' own `DeleteFileW` takes the disposition route, so this test drives the flag
//! directly as well as through `std::fs::remove_file`, which reaches `DeleteFileW` either way.
//!
//! The control is a plain open and close of a served file: closing must not delete.

use crate::fakedirector;

use fakedirector::{Fake, ReadStyle};
use std::os::windows::ffi::OsStrExt;
use vfs_shim::install;
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_FLAG_DELETE_ON_CLOSE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};

const HOST: &[u8] = b"host: the real file under the root";

fn open_and_close(path: &std::path::Path, flags: u32) -> bool {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | DELETE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            core::ptr::null(),
            OPEN_EXISTING,
            flags,
            core::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return false;
    }
    unsafe { CloseHandle(h) };
    true
}

#[test]
fn a_delete_on_close_open_deletes_at_the_director_when_closed() {
    isolate!();
    let base = std::env::temp_dir().join(format!("vfs-delete-on-close-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(root.join("data")).unwrap();
    for name in ["temp.esp", "kept.esp", "removed.esp"] {
        std::fs::write(root.join("data").join(name), HOST).unwrap();
    }

    let fake = fakedirector::install(
        &root,
        Fake::new()
            .with_dir("data")
            .with("data/temp.esp", b"temp".to_vec(), ReadStyle::Whole)
            .with("data/kept.esp", b"kept".to_vec(), ReadStyle::Whole)
            .with("data/removed.esp", b"removed".to_vec(), ReadStyle::Whole)
            .writable_under("data/"),
        0,
    );
    let hooks = install().expect("install");

    let temp_opened = open_and_close(
        &root.join("data").join("temp.esp"),
        FILE_FLAG_DELETE_ON_CLOSE,
    );
    let kept_opened = open_and_close(&root.join("data").join("kept.esp"), 0);
    let removed = std::fs::remove_file(root.join("data").join("removed.esp"));
    let removed_gone = std::fs::read(root.join("data").join("removed.esp")).is_err();
    drop(hooks);

    assert!(
        temp_opened,
        "the delete-on-close open of a served file failed"
    );
    assert_eq!(
        fake.tally.deletes("data/temp.esp"),
        1,
        "closing a FILE_DELETE_ON_CLOSE handle must send one OP_DELETE"
    );
    assert_eq!(
        fake.contents("data/temp.esp"),
        None,
        "the director still serves it"
    );

    assert!(kept_opened, "the plain open of a served file failed");
    assert_eq!(
        fake.tally.deletes("data/kept.esp"),
        0,
        "closing a handle opened without the flag deleted the file"
    );

    removed.expect("std::fs::remove_file of a served file");
    assert!(removed_gone, "the removed file still reads");
    assert_eq!(
        fake.contents("data/removed.esp"),
        None,
        "DeleteFileW deleted nothing"
    );

    for name in ["temp.esp", "kept.esp", "removed.esp"] {
        assert_eq!(
            std::fs::read(root.join("data").join(name)).ok().as_deref(),
            Some(HOST),
            "the real {name} under the root was deleted or changed"
        );
    }

    let _ = std::fs::remove_dir_all(&base);
}
