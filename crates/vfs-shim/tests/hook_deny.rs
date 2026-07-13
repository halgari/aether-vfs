//! Single-test binary: a tombstoned real file must be hidden by the hook.
use vfs_shim::{install, Engine};

#[test]
fn tombstone_hides_a_real_file_and_others_pass_through() {
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-shim-deny-{pid}"));
    std::fs::create_dir_all(&root).unwrap();

    // Two REAL files on disk under the managed root.
    let hidden = root.join("hidden.esp");
    let visible = root.join("visible.esp");
    std::fs::write(&hidden, b"SHOULD BE HIDDEN").unwrap();
    std::fs::write(&visible, b"SHOULD BE VISIBLE").unwrap();

    // Snapshot tombstones hidden.esp; says nothing about visible.esp.
    let snapshot = {
        use vfs_core::{build, EntryKind, InputEntry, Layer, LayerId};
        let tree = build(vec![Layer {
            id: LayerId(0),
            entries: vec![InputEntry {
                vpath: "hidden.esp".into(),
                kind: EntryKind::Tombstone,
                source: "".into(),
                size: 0,
                mtime: 0,
            }],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };

    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    // Tombstoned real file is hidden even though it exists on disk.
    let err = std::fs::read(&hidden).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);

    // A non-virtualized real file still reads (pass-through).
    assert_eq!(std::fs::read(&visible).unwrap(), b"SHOULD BE VISIBLE");
}
