//! Single-test binary: a relative name must resolve through the VFS on **every**
//! hook that decodes one.
//!
//! NT lets a caller name a file as (directory handle + relative name) instead of
//! an absolute path, and Win32 uses that form constantly: `CreateFileW("Data\X")`
//! reaches ntdll as the process's current-directory handle plus `Data\X`. A hook
//! that only understands absolute names does not *fail* on these — it decodes
//! nothing, declines to act, and the call proceeds to whatever is really on disk
//! behind the mount. Nothing is logged, no error is returned, and the file simply
//! appears not to exist.
//!
//! That cost a long debugging session: Skyrim reached its main menu with an empty
//! load order because every plugin lookup took this form, and the shipped tests
//! all used absolute paths, so the whole dimension was untested. This binary
//! covers it once per API rather than once, so closing the hole in one hook
//! cannot leave it open in another.
//!
//! Task 4: this binary installs the shim with **no director** attached
//! (`vfs_shim::install`, not a real launch). `NtCreateFile`/`NtOpenFile`
//! relative-name resolution, and anything built on top of an opened handle
//! (`std::fs::read`, `std::fs::metadata` — Rust's std opens a handle for
//! `metadata()` too, it does not call `NtQueryAttributesFile` directly), goes
//! through `Decision` (untouched by Task 4; that deletion is gate 4's), so
//! every read and `metadata()` assertion below is unchanged.
//!
//! What Task 4 did change: directory enumeration (`RootMap::merge_directory`,
//! deleted) and the *name-based, no-handle* attribute queries
//! (`NtQueryAttributesFile`, `NtQueryFullAttributesFile`,
//! `NtQueryInformationByName` — `RootMap::query_attributes`/`AttrDecision`,
//! deleted) now route to the director only. With no director attached here,
//! a CWD-relative `read_dir` no longer shows the mod-only file, and a
//! handle-relative raw attribute query no longer finds it either — both
//! assertions were flipped and are called out individually below.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

mod ntapi;
use ntapi::*;

const PAYLOAD: &[u8] = b"master-plugin-bytes";

#[test]
fn relative_names_resolve_on_every_decoding_hook() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-shim-relpath-{pid}"));
    let root = base.join("gameroot");
    let backing = base.join("backing");
    // `Data` exists for real, exactly like a staged game tree. A real marker
    // file keeps it non-empty (see the CWD-relative `read_dir` check below,
    // which needs at least one real entry to enumerate — Task 4 removed the
    // snapshot merge, so a directory with nothing real and nothing in the
    // overlay reports `STATUS_NO_MORE_FILES` on the very first scan, same as
    // a genuinely-empty real directory would once dot-entries are stripped;
    // that is orthogonal to what this test checks).
    std::fs::create_dir_all(root.join("Data")).unwrap();
    std::fs::write(root.join("Data").join("real_marker.txt"), b"m").unwrap();
    std::fs::create_dir_all(&backing).unwrap();
    let backing_file = backing.join("added.esm");
    std::fs::write(&backing_file, PAYLOAD).unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "Data/added.esm".into(),
                kind: EntryKind::File,
                source: backing_file.to_string_lossy().as_ref().into(),
                size: PAYLOAD.len() as u64,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = vfs_shim::Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = vfs_shim::install(engine).expect("install");

    // ── baseline: the absolute spelling, which already worked ───────────────
    let abs = root.join("Data").join("added.esm");
    assert_eq!(
        std::fs::read(&abs).expect("absolute read"),
        PAYLOAD,
        "absolute path must serve the virtual file"
    );

    // ── current-directory-relative, via the ordinary Win32 surface ───────────
    // Whether ntdll expands this against the CWD *string* or hands the kernel
    // the CWD *handle* is its choice and varies by path shape; either way the
    // caller must see the virtual file.
    std::env::set_current_dir(&root).expect("set cwd");
    assert_eq!(
        std::fs::read(r"Data\added.esm").expect("cwd-relative read"),
        PAYLOAD,
        "a CWD-relative open must resolve through the VFS"
    );
    // `std::fs::metadata` opens a handle (`Decision`-backed), so this is
    // unaffected by Task 4 — unchanged from before.
    assert_eq!(
        std::fs::metadata(r"Data\added.esm").expect("cwd-relative metadata").len(),
        PAYLOAD.len() as u64,
        "a CWD-relative stat must report the virtual size"
    );
    // `read_dir` enumerates via the handle-based directory hooks, which no
    // longer merge the snapshot in (Task 4 deleted `RootMap::merge_directory`),
    // so the mod-only file does not appear, flipped from "must include the
    // virtual file".
    let listed: Vec<String> = std::fs::read_dir("Data")
        .expect("cwd-relative read_dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !listed.iter().any(|n| n == "added.esm"),
        "a mod-added file leaked into a CWD-relative enumeration with no director: {listed:?}"
    );

    // ── handle-relative, exercised deterministically ────────────────────────
    // The Win32 calls above may or may not produce the handle form. These do,
    // unconditionally, so the (directory handle + name) path is really covered.
    let dir = open_dir(&root);
    assert!(!dir.is_null(), "could not open the root directory");

    // NtCreateFile
    let h = nt_create_relative(dir, r"Data\added.esm");
    assert!(h.0 >= 0, "NtCreateFile relative to a handle: status {:#x}", h.0);
    assert_eq!(read_all(h.1), PAYLOAD, "NtCreateFile served the wrong bytes");
    close(h.1);

    // NtOpenFile
    let h = nt_open_relative(dir, r"Data\added.esm");
    assert!(h.0 >= 0, "NtOpenFile relative to a handle: status {:#x}", h.0);
    assert_eq!(read_all(h.1), PAYLOAD, "NtOpenFile served the wrong bytes");
    close(h.1);

    // NtQueryAttributesFile — existence only, but that is what callers branch on.
    // No director attached: Task 4 deleted the local snapshot-answering
    // fallback these attribute hooks used to have, so a relative stat of a
    // virtual-only file must now fail, flipped from "status {st:#x}" >= 0 —
    // see the module doc comment.
    let (st, _attrs) = nt_query_attributes_relative(dir, r"Data\added.esm");
    assert!(st < 0, "NtQueryAttributesFile relative saw a virtual-only file with no director");

    // NtQueryFullAttributesFile — same flip.
    let (st, _size) = nt_query_full_attributes_relative(dir, r"Data\added.esm");
    assert!(
        st < 0,
        "NtQueryFullAttributesFile relative saw a virtual-only file with no director"
    );

    // NtQueryInformationByName — Windows 11 routes existence checks here; same flip.
    if let Some((st, _size)) = nt_query_by_name_relative(dir, r"Data\added.esm", 77) {
        assert!(
            st < 0,
            "NtQueryInformationByName(77) relative saw a virtual-only file with no director"
        );
    }

    // A name that exists in neither the VFS nor on disk must still say so.
    let (st, _) = nt_query_full_attributes_relative(dir, r"Data\absent.esm");
    assert!(st < 0, "a missing relative name must not report success");

    close(dir);

    // Leave the CWD somewhere stable for any later harness code.
    let _ = std::env::set_current_dir(std::env::temp_dir());
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
