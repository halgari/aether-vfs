//! Single-test binary: `OBJECT_ATTRIBUTES.RootDirectory` set to a real
//! directory handle the shim never saw opened.
//!
//! `hook_relative_paths.rs` already covers the handle-relative shape for a
//! handle the shim itself opened (and therefore already tracks). That leaves
//! a narrower, more dangerous case untested: a directory handle opened
//! *before* the shim was ever installed -- exactly what happens when a game
//! holds a handle to an ancestor of the managed root (`C:\Games`) and later
//! names a file only relative to it (`Skyrim\Data\a.esp`). The string a hook
//! sees is just the relative part; the root information lives in the handle,
//! not the string, so no amount of canonicalising the string alone can
//! recover it. Without an OS-backed fallback, a hook that cannot decode the
//! parent does not error -- it silently hands the *original* handle+name
//! straight to the real `NtCreateFile`, bypassing every VFS decision.
//!
//! Both directions matter here (see the gate's own history of one shipped
//! over-eager bypass): an ancestor handle must not leave an in-root child
//! unrouted (under-eager), and it must not cause a genuinely-outside sibling
//! to be misrouted through the VFS (over-eager).

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

mod ntapi;
use ntapi::*;

/// What the virtual (correct) file must read as.
const VIRTUAL_PAYLOAD: &[u8] = b"virtual-plugin-bytes";
/// What is actually sitting on disk at the very same relative spot. A real
/// bypass would return this instead -- proof the wrong content leaked,
/// not just that the call failed.
const HOST_LEAK_PAYLOAD: &[u8] = b"host-disk-leak-bytes-should-never-be-seen";
/// Content of a file that is genuinely outside every managed root; the
/// over-eager check requires this to come back byte-for-byte unmodified.
const OUTSIDE_PAYLOAD: &[u8] = b"genuinely-outside-real-bytes";

#[test]
fn handle_relative_open_via_a_handle_opened_before_injection() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-shim-unseen-handle-{pid}"));
    // `base` plays the role of `C:\Games`: an ancestor of the managed root,
    // itself genuinely outside it.
    let root = base.join("gameroot"); // plays `C:\Games\Skyrim`
    let backing_dir = base.join("backing");
    std::fs::create_dir_all(root.join("Data")).unwrap();
    std::fs::create_dir_all(&backing_dir).unwrap();

    // The real on-disk file a bypass would actually read, shadowed by the
    // virtual mapping below. If the vector stays open, this is what a
    // relative open returns instead of the virtual content.
    std::fs::write(root.join("Data").join("added.esm"), HOST_LEAK_PAYLOAD).unwrap();

    // A genuinely-outside sibling file, for the over-eager direction.
    std::fs::write(base.join("outside.txt"), OUTSIDE_PAYLOAD).unwrap();

    let backing_file = backing_dir.join("added.esm");
    std::fs::write(&backing_file, VIRTUAL_PAYLOAD).unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "Data/added.esm".into(),
                kind: EntryKind::File,
                source: backing_file.to_string_lossy().as_ref().into(),
                size: VIRTUAL_PAYLOAD.len() as u64,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    // ── open the ancestor handle BEFORE the shim exists ─────────────────────
    // No hooks are installed yet, so this `CreateFileW` reaches the real
    // `NtCreateFile` untouched: the shim's handle tables (and therefore
    // `path_of_handle`) will never contain this handle. This is the
    // "opened before injection" shape the fix has to cover -- the OS
    // fallback is the only way left to decode a relative open through it.
    let ancestor = open_dir(&base);
    assert!(!ancestor.is_null(), "could not open the ancestor directory");

    let engine = vfs_shim::Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = vfs_shim::install(engine).expect("install");

    // ── under-eager direction: the in-root child must resolve through the VFS ──
    let h = nt_create_relative(ancestor, r"gameroot\Data\added.esm");
    assert!(
        h.0 >= 0,
        "NtCreateFile relative to a pre-injection ancestor handle failed: status {:#x}",
        h.0
    );
    let bytes = read_all(h.1);
    close(h.1);
    assert_ne!(
        bytes, HOST_LEAK_PAYLOAD,
        "the real on-disk file leaked through an unrouted handle-relative open \
         -- the ancestor-handle vector is still open"
    );
    assert_eq!(
        bytes, VIRTUAL_PAYLOAD,
        "a handle-relative open through a pre-injection ancestor handle did not \
         resolve to the virtual content"
    );

    // ── over-eager direction: a genuinely-outside sibling must stay outside ──
    let h = nt_create_relative(ancestor, r"outside.txt");
    assert!(
        h.0 >= 0,
        "NtCreateFile for a genuinely outside-root relative name failed: status {:#x}",
        h.0
    );
    let bytes = read_all(h.1);
    close(h.1);
    assert_eq!(
        bytes, OUTSIDE_PAYLOAD,
        "a file outside every managed root was not served as itself -- the OS \
         fallback over-matched an outside path as under-root"
    );

    close(ancestor);
    let _ = std::fs::remove_dir_all(&base);
}

/// Opens a directory handle the way Win32 does (`FILE_FLAG_BACKUP_SEMANTICS`).
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
