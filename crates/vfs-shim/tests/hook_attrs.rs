//! Single-test binary: path-based attribute queries reflect the VFS.
use std::ffi::c_void;
use vfs_shim::{install, Engine};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileAttributesExW, GetFileAttributesW, GetFileExInfoStandard, FILE_ATTRIBUTE_DIRECTORY,
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

    // Virtual file exists (not INVALID) and is not a directory.
    let a = unsafe { GetFileAttributesW(wide(vfile.to_str().unwrap()).as_ptr()) };
    assert_ne!(a, INVALID_FILE_ATTRIBUTES, "virtual file should have attributes");
    assert_eq!(a & FILE_ATTRIBUTE_DIRECTORY, 0, "virtual file must not be a dir");

    // Virtual dir has the DIRECTORY bit.
    let d = unsafe { GetFileAttributesW(wide(vdir.to_str().unwrap()).as_ptr()) };
    assert_ne!(d, INVALID_FILE_ATTRIBUTES);
    assert_ne!(d & FILE_ATTRIBUTE_DIRECTORY, 0, "virtual dir must be a dir");

    // Tombstoned real file is hidden.
    let g = unsafe { GetFileAttributesW(wide(gone.to_str().unwrap()).as_ptr()) };
    assert_eq!(g, INVALID_FILE_ATTRIBUTES, "tombstoned file must be hidden");

    // Non-virtual real file passes through.
    let r = unsafe { GetFileAttributesW(wide(real.to_str().unwrap()).as_ptr()) };
    assert_ne!(r, INVALID_FILE_ATTRIBUTES, "real file should pass through");

    // Full attributes report the snapshot's size.
    let mut data: WIN32_FILE_ATTRIBUTE_DATA = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileAttributesExW(
            wide(vfile.to_str().unwrap()).as_ptr(),
            GetFileExInfoStandard,
            &mut data as *mut _ as *mut c_void,
        )
    };
    assert_ne!(ok, 0, "GetFileAttributesExW should succeed for the virtual file");
    let size = ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64;
    assert_eq!(size, 1234, "reported size should match the snapshot");
}
