//! Single-test binary: `std::fs::read_dir` sees the merged VFS view.
use vfs_shim::{install, Engine};

#[test]
fn read_dir_reflects_the_merged_vfs() {
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-shim-direnum-{pid}"));
    // Backing files live OUTSIDE the root so they do not appear in the listing.
    let backing_dir =
        std::env::temp_dir().join(format!("vfs-shim-direnum-backing-{pid}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backing_dir).unwrap();

    // Real on-disk contents of the enumerated directory.
    std::fs::write(root.join("real_a.txt"), b"a").unwrap();
    std::fs::write(root.join("real_b.txt"), b"b").unwrap();
    std::fs::write(root.join("over.esp"), vec![0u8; 3]).unwrap(); // overridden
    std::fs::write(root.join("gone.esp"), b"x").unwrap(); // tombstoned
    std::fs::create_dir_all(root.join("realdir")).unwrap();

    // Backing files for the mod override / add.
    let over_backing = backing_dir.join("over.esp");
    std::fs::write(&over_backing, vec![0u8; 4096]).unwrap();
    let add_backing = backing_dir.join("added.esp");
    std::fs::write(&add_backing, vec![0u8; 10]).unwrap();

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
                e("added.esp", EntryKind::File, add_backing.to_str().unwrap(), 10),
                e("over.esp", EntryKind::File, over_backing.to_str().unwrap(), 4096),
                e("gone.esp", EntryKind::Tombstone, "", 0),
                e("vdir", EntryKind::Dir, "", 0),
            ],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    let mut names: Vec<String> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    assert!(names.contains(&"added.esp".to_string()), "mod-added missing: {names:?}");
    assert!(names.contains(&"real_a.txt".to_string()), "{names:?}");
    assert!(names.contains(&"real_b.txt".to_string()), "{names:?}");
    assert!(names.contains(&"over.esp".to_string()), "{names:?}");
    assert!(names.contains(&"realdir".to_string()), "{names:?}");
    assert!(names.contains(&"vdir".to_string()), "virtual dir missing: {names:?}");
    assert!(!names.contains(&"gone.esp".to_string()), "tombstone shown: {names:?}");

    // Override wins: over.esp reports the mod size (4096), not the real 3.
    let over = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap())
        .find(|e| e.file_name().to_string_lossy() == "over.esp")
        .unwrap();
    assert_eq!(over.metadata().unwrap().len(), 4096, "override size should win");
}
