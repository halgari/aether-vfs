//! Single-test binary: installing a process-global NtCreateFile detour must not
//! race other tests, so this hook test stands alone.

use vfs_shim::{install, Engine};

#[test]
fn hooked_open_reads_the_backing_file() {
    // Unique temp root for this run.
    let root = std::env::temp_dir().join(format!("vfs-shim-it-{}", std::process::id()));
    let backing_dir = root.join("backing");
    std::fs::create_dir_all(&backing_dir).unwrap();
    let backing = backing_dir.join("real.esp");
    std::fs::write(&backing, b"BACKING BYTES OK").unwrap();

    // The virtual path lives directly under the root and does NOT exist on disk.
    let virtual_path = root.join("virtual.esp");
    assert!(std::fs::read(&virtual_path).is_err(), "virtual path must not pre-exist");

    // Build a snapshot mapping vpath `virtual.esp` -> the backing file's abs path.
    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let source = backing.to_str().unwrap().to_string();
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "virtual.esp".into(),
                kind: EntryKind::File,
                source: source.as_str().into(),
                size: 16,
                mtime: 1,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    // Root as a Win32 path (RootMap::new accepts it); NtCreateFile will see the
    // `\??\` NT form and RootMap matches component-wise.
    let root_str = root.to_str().unwrap();
    let engine = Engine::new(root_str, snapshot).unwrap();

    let _guard = install(engine).expect("hook install");

    // Open the VIRTUAL path — the hook redirects to the backing file.
    let content = std::fs::read_to_string(&virtual_path).expect("redirected open");
    assert_eq!(content, "BACKING BYTES OK");

    // _guard drops here, disabling the hook.
}
