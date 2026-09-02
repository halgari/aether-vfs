//! The breadcrumb must be readable **from outside the process**.
//!
//! That is the entire reason it is a file-backed mapping rather than a static:
//! the failure it exists for is an injected process wedged inside a hook with
//! zero CPU, one thread, and immune to `TerminateProcess`. Such a process cannot
//! be attached to, so a diagnostic it keeps privately in its own heap is
//! unreadable at exactly the moment it is needed.
//!
//! So this test does not merely call `snapshot()`; it parses the bytes off disk
//! the way a watchdog would.
#![cfg(windows)]

use vfs_shim::{install, Engine};

const NONE: u32 = u32::MAX;
const MAGIC: u32 = 0x4252_4342;

fn read_file_breadcrumb(path: &std::path::Path) -> (u32, u32, u64, u64, u32) {
    let b = std::fs::read(path).expect("breadcrumb file must exist once installed");
    assert!(b.len() >= 28, "breadcrumb file is short: {} bytes", b.len());
    let g = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let g64 = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    (g(0), g(4), g64(8), g64(16), g(24))
}

#[test]
fn an_outside_reader_can_see_which_hook_the_process_is_in() {
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("vfs-bcrb-{pid}"));
    std::fs::create_dir_all(&dir).unwrap();
    let base = dir.join("breadcrumb.bin");
    // init() inserts the pid so concurrent injected processes cannot collide.
    let crumb = dir.join(format!("breadcrumb.{pid}.bin"));
    let _ = std::fs::remove_file(&crumb);
    // Set before install: `init()` runs there, deliberately before the detours
    // go live, because creating this file is I/O that would otherwise re-enter
    // the very hooks being installed.
    std::env::set_var("VFS_SHIM_BREADCRUMB", &base);

    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let backing = dir.join("backing.dat");
    std::fs::write(&backing, b"breadcrumbed").unwrap();

    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "mod.esp".into(),
                kind: EntryKind::File,
                source: backing.to_str().unwrap().into(),
                size: 12,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    assert!(
        vfs_shim::breadcrumb::is_active(),
        "a breadcrumb path was set, so the mapping must be live"
    );

    let (magic, _, entries_before, _, _) = read_file_breadcrumb(&crumb);
    assert_eq!(magic, MAGIC, "magic must be stamped so a zeroed page is not misread");

    // Drive some hooked file activity.
    let content = std::fs::read(root.join("mod.esp")).expect("read the virtual file");
    assert_eq!(content, b"breadcrumbed");

    let (_, current, entries_after, exits, last_done) = read_file_breadcrumb(&crumb);

    assert!(
        entries_after > entries_before,
        "hook entries must be visible from outside: {entries_before} -> {entries_after}"
    );
    assert!(exits > 0, "completed hooks must be counted too");
    assert_ne!(
        last_done, NONE,
        "the last completed hook must be recorded, or a stall cannot be attributed"
    );
    // `current` is deliberately NOT asserted against the value read from the
    // file. Reading that file is itself a hooked call, so an in-process reader
    // observes `current == Hook::Read` — its own read in flight. Measured: this
    // assertion failed with left=4 (NtReadFile) before the cause was understood.
    //
    // That self-observation is unique to reading from inside the instrumented
    // process, which is not how the diagnostic is used: the real reader is a
    // separate watchdog, for which the file read is ordinary unhooked I/O. The
    // in-process equivalent is `snapshot()`, which touches only the mapping and
    // performs no I/O, so it can see the quiescent state.
    let (s_current, s_entries, s_exits, s_last) = vfs_shim::breadcrumb::snapshot().unwrap();
    assert_eq!(
        s_current, NONE,
        "current must clear on exit, or a sample cannot distinguish 'inside' from 'finished'"
    );
    assert!(s_entries >= entries_after, "{s_entries} < {entries_after}");
    assert!(s_exits >= exits, "{s_exits} < {exits}");
    assert_ne!(s_last, NONE);
    let _ = (current, last_done);

    std::env::remove_var("VFS_SHIM_BREADCRUMB");
}
