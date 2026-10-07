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
//!
//! Two more pin the edges: a delete the director refuses at close is counted (the close itself
//! cannot fail, and `DeleteFileW` returns TRUE regardless), and a handle whose file was already
//! deleted through a set-info disposition does not delete again at close — which would delete a
//! file recreated under the same name in between.

use crate::fakedirector;

use fakedirector::{Fake, ReadStyle};
use std::os::windows::ffi::OsStrExt;
use vfs_shim::{delete_on_close_refused_count, install};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_FLAG_DELETE_ON_CLOSE, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};

const HOST: &[u8] = b"host: the real file under the root";

fn open(path: &std::path::Path, flags: u32) -> Option<windows_sys::Win32::Foundation::HANDLE> {
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
    (h != INVALID_HANDLE_VALUE).then_some(h)
}

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

/// A delete-on-close the director refuses (the file is served by a read-only layer) leaves the
/// file, still closes the handle successfully, and is counted.
#[test]
fn a_refused_delete_on_close_is_counted_and_the_handle_still_closes() {
    isolate!();
    let base = std::env::temp_dir().join(format!("vfs-doc-refused-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(root.join("data")).unwrap();
    // Served, but by no writable layer: `OP_DELETE` answers `ST_READ_ONLY`.
    let fake = fakedirector::install(
        &root,
        Fake::new()
            .with_dir("data")
            .with("data/locked.esp", b"locked".to_vec(), ReadStyle::Whole),
        0,
    );
    let hooks = install().expect("install");

    let before = delete_on_close_refused_count();
    let h = open(
        &root.join("data").join("locked.esp"),
        FILE_FLAG_DELETE_ON_CLOSE,
    )
    .expect("the delete-on-close open of a served file failed");
    let closed = unsafe { CloseHandle(h) };
    let after = delete_on_close_refused_count();
    drop(hooks);

    assert_ne!(
        closed, 0,
        "the close itself must succeed: NtClose cannot report a refusal"
    );
    assert_eq!(
        fake.tally.deletes("data/locked.esp"),
        1,
        "the close must ask the director"
    );
    assert_eq!(
        fake.contents("data/locked.esp").as_deref(),
        Some(&b"locked"[..]),
        "the refused delete left the file in place"
    );
    assert_eq!(after - before, 1, "the refusal must be counted");

    let _ = std::fs::remove_dir_all(&base);
}

/// A set-info delete through a delete-on-close handle deletes once. A file created under the
/// same name before the handle closes survives the close.
#[test]
fn a_set_info_delete_clears_delete_on_close_so_the_close_does_not_delete_again() {
    use windows_sys::Win32::Storage::FileSystem::{
        FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
    };
    isolate!();
    let base = std::env::temp_dir().join(format!("vfs-doc-once-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    std::fs::create_dir_all(root.join("data")).unwrap();
    let fake = fakedirector::install(
        &root,
        Fake::new()
            .with_dir("data")
            .with("data/again.esp", b"first".to_vec(), ReadStyle::Whole)
            .writable_under("data/"),
        0,
    );
    let hooks = install().expect("install");

    let path = root.join("data").join("again.esp");
    let h = open(&path, FILE_FLAG_DELETE_ON_CLOSE).expect("delete-on-close open");
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    let set = unsafe {
        SetFileInformationByHandle(
            h,
            FileDispositionInfo,
            &info as *const _ as *const core::ffi::c_void,
            core::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    // Recreated under the same name while the first handle is still open.
    let recreated = std::fs::write(&path, b"second");
    let closed = unsafe { CloseHandle(h) };
    let after = std::fs::read(&path);
    drop(hooks);

    assert_ne!(set, 0, "the disposition delete must succeed");
    recreated.expect("recreate under the same name");
    assert_ne!(closed, 0);
    assert_eq!(
        fake.tally.deletes("data/again.esp"),
        1,
        "the close sent a second OP_DELETE after the disposition had already deleted the file"
    );
    assert_eq!(
        after.ok().as_deref(),
        Some(&b"second"[..]),
        "the recreated file was deleted"
    );
    assert_eq!(
        fake.contents("data/again.esp").as_deref(),
        Some(&b"second"[..])
    );

    let _ = std::fs::remove_dir_all(&base);
}
