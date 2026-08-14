//! Single-test binary: both directory-enumeration entry points show one view.
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
//! Task 4: this binary installs the shim with **no director** attached
//! (`vfs_shim::install`, not a real launch). Before Task 4, a directory
//! listing without a director still merged in the snapshot's virtual
//! children and hid its tombstones (`RootMap::merge_directory`). That local
//! merge is deleted: with no director, a listing is exactly the real
//! directory (plus any write-overlay entries, gate 4's mechanism, unaffected
//! here). So `added.esm` (mod-only) no longer appears, and `hidden.esp`
//! (tombstoned only in the snapshot) is no longer hidden — both assertions
//! below were flipped for that reason. The two-entry-point agreement itself —
//! this test's actual point — is unchanged and still the thing being proven:
//! whatever the real directory contains, both entry points must show it
//! identically.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

mod ntapi;
use ntapi::*;

#[test]
fn classic_and_ex_enumeration_agree() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-shim-enumparity-{pid}"));
    let root = base.join("gameroot");
    let backing = base.join("backing");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backing).unwrap();

    // A real file, a VFS-only file, and a real file hidden by a tombstone: the
    // three cases where the merged view differs from what is on disk.
    std::fs::write(root.join("real.txt"), b"r").unwrap();
    std::fs::write(root.join("hidden.esp"), b"h").unwrap();
    let add_backing = backing.join("added.esm");
    std::fs::write(&add_backing, vec![0u8; 7]).unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let e = |vpath: &str, kind: EntryKind, source: &str, size: u64| InputEntry {
            vpath: vpath.into(),
            kind,
            source: source.into(),
            size,
            mtime: 0,
        };
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![
                e("added.esm", EntryKind::File, add_backing.to_str().unwrap(), 7),
                e("hidden.esp", EntryKind::Tombstone, "", 0),
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = vfs_shim::Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = vfs_shim::install(engine).expect("install");

    // `read_dir` goes through NtQueryDirectoryFileEx.
    let mut via_ex: Vec<String> = std::fs::read_dir(&root)
        .expect("read_dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();

    let dir = open_dir(&root);
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
    // *consistently wrong* still fails. With no director, this is exactly the
    // real directory: `real.txt` and `hidden.esp` (real files) show, and
    // `added.esm` (mod-only, snapshot only) does not — see the module doc
    // comment for why this flipped from the old merged-view expectations.
    assert!(
        via_classic.iter().any(|n| n == "real.txt"),
        "real.txt missing: {via_classic:?}"
    );
    assert!(
        !via_classic.iter().any(|n| n == "added.esm"),
        "a mod-added file leaked in without a director consulting the snapshot: {via_classic:?}"
    );
    assert!(
        via_classic.iter().any(|n| n == "hidden.esp"),
        "no director means no snapshot tombstone — the real file must not be hidden: {via_classic:?}"
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
