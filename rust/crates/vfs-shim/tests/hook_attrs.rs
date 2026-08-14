//! Single-test binary: path-based attribute queries reflect the VFS.
//!
//! Task 4: this binary installs the shim with **no director** attached
//! (`vfs_shim::install`, not a real launch). Before Task 4, attribute queries
//! fell back to answering locally from the published snapshot
//! (`RootMap::query_attributes`/`AttrDecision`), so a virtual file/dir was
//! visible and a tombstoned real file was hidden even with nothing to
//! consult. That local-answering path is deleted — attribute queries now
//! route to the director only (`hook.rs::fuse_path_attr`) — so with no
//! director, none of that happens any more: a virtual path is (correctly)
//! invisible, and a tombstoned real file is (correctly, for this harness)
//! visible, since nothing here has been told to hide it. The assertions below
//! were flipped for exactly that reason and documented at each site; the
//! "non-virtual real file passes through" case is unchanged, since it never
//! depended on the deleted mechanism.
use std::ffi::c_void;
use vfs_shim::{install, Engine};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileAttributesExW, GetFileAttributesW, GetFileExInfoStandard,
    INVALID_FILE_ATTRIBUTES, WIN32_FILE_ATTRIBUTE_DATA,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[test]
fn attribute_queries_reflect_the_vfs() {
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-shim-attrs-{pid}"));
    let backing_dir = root.join("backing");
    std::fs::create_dir_all(&backing_dir).unwrap();

    // Real files on disk under the root.
    let real = root.join("real.esp"); // not in snapshot -> pass through
    let gone = root.join("gone.esp"); // tombstoned -> hidden
    std::fs::write(&real, b"real").unwrap();
    std::fs::write(&gone, b"gone").unwrap();
    let backing = backing_dir.join("mod.esp");
    std::fs::write(&backing, vec![0u8; 1234]).unwrap();

    // Virtual paths (absent on disk).
    let vfile = root.join("mod.esp");
    let vdir = root.join("moddir");

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
                e("mod.esp", EntryKind::File, backing.to_str().unwrap(), 1234),
                e("moddir", EntryKind::Dir, "", 0),
                e("gone.esp", EntryKind::Tombstone, "", 0),
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    // No director: nothing answers for a virtual-only path any more (flipped
    // from "virtual file should have attributes" — see the module doc comment).
    let a = unsafe { GetFileAttributesW(wide(vfile.to_str().unwrap()).as_ptr()) };
    assert_eq!(
        a, INVALID_FILE_ATTRIBUTES,
        "a virtual file was visible with no director attached"
    );

    // Same for a virtual directory (flipped from "virtual dir must be a dir").
    let d = unsafe { GetFileAttributesW(wide(vdir.to_str().unwrap()).as_ptr()) };
    assert_eq!(
        d, INVALID_FILE_ATTRIBUTES,
        "a virtual directory was visible with no director attached"
    );

    // No director: nothing enforces the snapshot's tombstone any more
    // (flipped from "tombstoned file must be hidden").
    let g = unsafe { GetFileAttributesW(wide(gone.to_str().unwrap()).as_ptr()) };
    assert_ne!(
        g, INVALID_FILE_ATTRIBUTES,
        "a tombstoned real file was hidden with no director attached to enforce it"
    );

    // Non-virtual real file passes through — unaffected by Task 4.
    let r = unsafe { GetFileAttributesW(wide(real.to_str().unwrap()).as_ptr()) };
    assert_ne!(r, INVALID_FILE_ATTRIBUTES, "real file should pass through");

    // No director: GetFileAttributesExW must fail for the virtual file too
    // (flipped from "should succeed ... report the snapshot's size").
    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileAttributesExW(
            wide(vfile.to_str().unwrap()).as_ptr(),
            GetFileExInfoStandard,
            &mut data as *mut _ as *mut c_void,
        )
    };
    assert_eq!(
        ok, 0,
        "GetFileAttributesExW succeeded for a virtual file with no director attached"
    );
}
