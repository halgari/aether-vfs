//! Runs in its own process: a relative name must resolve through the VFS on **every**
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
//! Every relative name below resolves against `Data`, a directory the (fake) director serves,
//! so each open, stat and listing through it is the director's answer. Before task C8 this
//! binary ran with no director: `Data` had to be overlay-backed just to be openable, and the
//! name-based attribute queries and the CWD-relative listing had been flipped to "nothing
//! answers". Through the director they make the original claims again: a relative stat finds
//! the served file, and a relative listing includes it.
//!
//! Multi-component relative decoding (`r"Sub\added2.esm"`, a name with its own interior
//! separator) is covered for the CWD-relative and handle-relative shapes: that is the class of
//! the project's own empty-load-order bug.
use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

use crate::fakedirector;
use crate::ntapi;
use fakedirector::{Fake, ReadStyle};
use ntapi::*;

const PAYLOAD: &[u8] = b"master-plugin-bytes";
/// Second file, one level deeper than `Data\`, so at least one relative open
/// below has to decode a name with an interior separator of its own
/// (`r"Sub\added2.esm"`), not just a bare filename — see the module doc
/// comment for why that class needs its own dedicated coverage.
const PAYLOAD2: &[u8] = b"multi-component-relative-bytes";

#[test]
fn relative_names_resolve_on_every_decoding_hook() {
    isolate!();
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-shim-relpath-{pid}"));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("gameroot");
    std::fs::create_dir_all(root.join("Data")).unwrap();

    fakedirector::install(
        &root,
        Fake::new()
            .with_dir("data")
            .with_dir("data/sub")
            .with("data/added.esm", PAYLOAD.to_vec(), ReadStyle::Whole)
            .with("data/sub/added2.esm", PAYLOAD2.to_vec(), ReadStyle::Whole)
            .with("data/real_marker.txt", b"m".to_vec(), ReadStyle::Whole),
        0,
    );
    let _guard = vfs_shim::install().expect("install");

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
    let data_dir = root.join("Data");
    std::env::set_current_dir(&data_dir).expect("set cwd");
    assert_eq!(
        std::fs::read("added.esm").expect("cwd-relative read"),
        PAYLOAD,
        "a CWD-relative open must resolve through the VFS"
    );
    assert_eq!(
        std::fs::metadata("added.esm")
            .expect("cwd-relative metadata")
            .len(),
        PAYLOAD.len() as u64,
        "a CWD-relative stat must report the virtual size"
    );
    // Multi-component: the relative name itself has an interior separator.
    assert_eq!(
        std::fs::read(r"Sub\added2.esm").expect("multi-component cwd-relative read"),
        PAYLOAD2,
        "a multi-component CWD-relative open must resolve through the VFS"
    );
    // `read_dir` enumerates via the handle-based directory hooks: the director's listing.
    let listed: Vec<String> = std::fs::read_dir(".")
        .expect("cwd-relative read_dir")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        listed.iter().any(|n| n == "added.esm"),
        "a CWD-relative enumeration must include the served file: {listed:?}"
    );
    assert!(
        listed.iter().any(|n| n == "real_marker.txt"),
        "a CWD-relative enumeration must include every served entry: {listed:?}"
    );

    // ── handle-relative, exercised deterministically ────────────────────────
    // The Win32 calls above may or may not produce the handle form. These do,
    // unconditionally, so the (directory handle + name) path is really
    // covered.
    let dir = open_dir(&data_dir);
    assert!(!dir.is_null(), "could not open the Data directory");

    // NtCreateFile
    let h = nt_create_relative(dir, "added.esm");
    assert!(
        h.0 >= 0,
        "NtCreateFile relative to a handle: status {:#x}",
        h.0
    );
    assert_eq!(
        read_all(h.1),
        PAYLOAD,
        "NtCreateFile served the wrong bytes"
    );
    close(h.1);

    // NtOpenFile
    let h = nt_open_relative(dir, "added.esm");
    assert!(
        h.0 >= 0,
        "NtOpenFile relative to a handle: status {:#x}",
        h.0
    );
    assert_eq!(read_all(h.1), PAYLOAD, "NtOpenFile served the wrong bytes");
    close(h.1);

    // Multi-component handle-relative, on both APIs.
    let h = nt_create_relative(dir, r"Sub\added2.esm");
    assert!(
        h.0 >= 0,
        "NtCreateFile multi-component relative to a handle: status {:#x}",
        h.0
    );
    assert_eq!(
        read_all(h.1),
        PAYLOAD2,
        "NtCreateFile multi-component relative served the wrong bytes"
    );
    close(h.1);

    let h = nt_open_relative(dir, r"Sub\added2.esm");
    assert!(
        h.0 >= 0,
        "NtOpenFile multi-component relative to a handle: status {:#x}",
        h.0
    );
    assert_eq!(
        read_all(h.1),
        PAYLOAD2,
        "NtOpenFile multi-component relative served the wrong bytes"
    );
    close(h.1);

    // NtQueryAttributesFile — existence only, but that is what callers branch on.
    let (st, _attrs) = nt_query_attributes_relative(dir, "added.esm");
    assert!(st >= 0, "NtQueryAttributesFile relative: status {st:#x}");

    // NtQueryFullAttributesFile — existence and size.
    let (st, size) = nt_query_full_attributes_relative(dir, "added.esm");
    assert!(
        st >= 0,
        "NtQueryFullAttributesFile relative: status {st:#x}"
    );
    assert_eq!(
        size,
        PAYLOAD.len() as i64,
        "NtQueryFullAttributesFile relative: wrong size"
    );

    // NtQueryInformationByName — Windows 11 routes existence checks here.
    if let Some((st, size)) = nt_query_by_name_relative(dir, "added.esm", 77) {
        assert!(
            st >= 0,
            "NtQueryInformationByName(77) relative: status {st:#x}"
        );
        assert_eq!(
            size,
            PAYLOAD.len() as i64,
            "NtQueryInformationByName(77): wrong size"
        );
    }

    // A name that exists in neither the VFS nor on disk must still say so.
    let (st, _) = nt_query_full_attributes_relative(dir, "absent.esm");
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
