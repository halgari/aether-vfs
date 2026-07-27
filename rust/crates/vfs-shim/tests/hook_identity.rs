//! Single-test binary: a redirected virtual file reports its VIRTUAL path.
use std::os::windows::io::AsRawHandle;
use vfs_shim::{install, Engine};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

#[test]
fn redirected_file_reports_virtual_path() {
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-shim-ident-{pid}"));
    let backing_dir = std::env::temp_dir().join(format!("vfs-shim-ident-backing-{pid}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backing_dir).unwrap();

    // Backing file with a DISTINCT name so we can tell the two paths apart.
    let backing = backing_dir.join("backing_blob.dat");
    std::fs::write(&backing, b"the-real-bytes").unwrap();

    // Virtual file, absent on disk, redirects to the backing blob.
    let vfile = root.join("mod.esp");

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "mod.esp".into(),
                kind: EntryKind::File,
                source: backing.to_str().unwrap().into(),
                size: 14,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    // Open the virtual path -> redirected; content comes from the backing file.
    let f = std::fs::File::open(&vfile).expect("open redirected virtual file");
    let content = std::fs::read(&vfile).unwrap();
    assert_eq!(content, b"the-real-bytes");

    let h = f.as_raw_handle() as HANDLE;
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, 0) };
    assert!(n > 0, "GetFinalPathNameByHandleW failed");
    let final_path = String::from_utf16_lossy(&buf[..n as usize]).to_lowercase();

    assert!(final_path.contains("mod.esp"), "should report virtual name: {final_path}");
    assert!(!final_path.contains("backing_blob"), "must NOT leak backing name: {final_path}");
}
