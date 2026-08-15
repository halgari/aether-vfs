//! **A directory opened with write access must still open** (gate 4, Task 6).
//!
//! `CreateFileW(dir, GENERIC_READ | GENERIC_WRITE, …, OPEN_EXISTING,
//! FILE_FLAG_BACKUP_SEMANTICS, …)` is the ordinary way to get a handle to a
//! directory on Windows — enumeration, `SetFileTime`, reparse-point work, and
//! anything that later calls `GetFinalPathNameByHandleW` all do it. On a
//! directory handle the access bits `is_write_open` reads as write access
//! (`0x0002 | 0x0004`) are `FILE_ADD_FILE` and `FILE_ADD_SUBDIRECTORY`, not
//! `FILE_WRITE_DATA`/`FILE_APPEND_DATA`; nothing in the mask distinguishes
//! them.
//!
//! So the shim classified those opens as writes and asked the director for
//! `OPEN_WRITE`, which no provider can give for a directory —
//! `DiskProvider::open` runs `OpenOptions::new().read(true).write(true)` on
//! it. Until gate 4's Task 5 the resulting refusal fell through to real disk
//! and the caller never noticed. Now it is the caller's answer:
//! `ERROR_GEN_FAILURE` from a writable mount, `ERROR_ACCESS_DENIED` from a
//! read-only one, for an operation NTFS answers without complaint.
//!
//! Both refusals are covered, because the fix must not be a patch for one
//! provider's error code. Directory *creates* are not: `try_fuse_mkdir` takes
//! those before `try_fuse_create` ever runs.
//!
//! Its own binary — the detours, `ENGINE` and the `FuseClient` are all
//! process-global and resolve once.

mod fakedirector;

use std::ffi::c_void;
use fakedirector::Fake;
use vfs_shim::{install, Engine};

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_SHARE_ALL: u32 = 0x0000_0007;
const OPEN_EXISTING: u32 = 3;
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const INVALID_HANDLE_VALUE: *mut c_void = usize::MAX as *mut c_void;

extern "system" {
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        sa: *mut c_void,
        disposition: u32,
        flags: u32,
        template: *mut c_void,
    ) -> *mut c_void;
    fn CloseHandle(h: *mut c_void) -> i32;
    fn GetLastError() -> u32;
}

/// `CreateFileW` with backup semantics and write access — the shape under
/// test. Returns `Ok(())` on success, `Err(win32_error)` otherwise.
fn open_directory_for_write(path: &std::path::Path) -> Result<(), u32> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let h = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_ALL,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(unsafe { GetLastError() });
    }
    unsafe { CloseHandle(h) };
    Ok(())
}

use std::os::windows::ffi::OsStrExt;

#[test]
fn a_directory_under_a_managed_root_opens_with_write_access() {
    let base = std::env::temp_dir().join(format!("vfs-dirwrite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("root");
    let overlay = base.join("overlay");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&overlay).unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "unrelated.txt".into(),
                kind: EntryKind::File,
                source: r"D:\nowhere\unrelated.txt".into(),
                size: 0,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    // `data` sits outside every writable prefix (read-only mount:
    // `ST_READ_ONLY`); `write/sub` sits inside one (writable mount, which
    // still cannot open a directory read+write: `ST_IO_ERROR`). One fix has
    // to cover both, or it is a patch for one provider's error code.
    fakedirector::install(
        &root,
        Fake::new()
            .with_dir("data")
            .with_dir("write/sub")
            .writable_under("write/"),
        0,
    );

    let engine =
        Engine::with_overlay(root.to_str().unwrap(), overlay.to_str().unwrap(), snapshot).unwrap();
    let hooks = install(engine).expect("install");

    let read_only_dir = open_directory_for_write(&root.join("Data"));
    let writable_dir = open_directory_for_write(&root.join("write").join("sub"));
    // The control: a path that genuinely is not there must still fail. Without
    // it, a downgrade that opened *everything* as a directory would pass the
    // two assertions above.
    let absent = open_directory_for_write(&root.join("no-such-dir"));

    drop(hooks);

    assert_eq!(
        read_only_dir,
        Ok(()),
        "a directory served by a read-only mount must open with backup-semantics write \
         access; ERROR_ACCESS_DENIED (5) here is the pre-fix answer"
    );
    assert_eq!(
        writable_dir,
        Ok(()),
        "a directory served by a writable mount must open too; ERROR_GEN_FAILURE (31) here \
         is the pre-fix answer — the writable provider tried to open the directory read+write"
    );
    assert!(
        absent.is_err(),
        "a path no provider serves must still fail: {absent:?}"
    );

    let _ = std::fs::remove_dir_all(&base);
}
