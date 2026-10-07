//! Runs in its own process: a virtual file reports its VIRTUAL path.
//!
//! The file is served by the (fake) director, so its handle is synthetic and
//! `GetFinalPathNameByHandleW` is answered by the shim (`NtQueryObject` on Wine,
//! `NtQueryInformationFile` on Windows) from the path it was opened as. Before
//! task C8 this was a snapshot redirect to a real backing file, and the claim was
//! that the backing file's name did not leak; the claim that survives is that the
//! name reported is the virtual one.
use crate::fakedirector;

use fakedirector::{Fake, ReadStyle};
use std::os::windows::io::AsRawHandle;
use vfs_shim::install;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::GetFinalPathNameByHandleW;

#[test]
fn redirected_file_reports_virtual_path() {
    isolate!();
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-shim-ident-{pid}"));
    std::fs::create_dir_all(&root).unwrap();

    // Virtual file, absent on disk, served by the director.
    let vfile = root.join("mod.esp");
    fakedirector::install(
        &root,
        Fake::new().with("mod.esp", b"the-real-bytes".to_vec(), ReadStyle::Whole),
        0,
    );
    let _guard = install().expect("install");

    let f = std::fs::File::open(&vfile).expect("open the virtual file");
    let content = std::fs::read(&vfile).unwrap();
    assert_eq!(content, b"the-real-bytes");

    let h = f.as_raw_handle() as HANDLE;
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetFinalPathNameByHandleW(h, buf.as_mut_ptr(), buf.len() as u32, 0) };
    assert!(n > 0, "GetFinalPathNameByHandleW failed");
    let final_path = String::from_utf16_lossy(&buf[..n as usize]).to_lowercase();

    let expect = root.join("mod.esp").to_string_lossy().to_lowercase();
    assert!(
        final_path.ends_with(&expect),
        "should report the virtual path {expect}: {final_path}"
    );
}
