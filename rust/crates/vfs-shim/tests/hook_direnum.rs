//! Single-test binary: without a director, `std::fs::read_dir` shows exactly
//! the real directory (plus any overlay) — the local snapshot no longer
//! contributes anything.
//!
//! Before Task 4, `RootMap::merge_directory` blended the snapshot's virtual
//! children into whatever the OS returned for a directory the shim could not
//! (or, in this harness, did not) ask a real director about. That let a
//! locally-improvised composition stand in for "the provider graph says", the
//! thing the game actually needs to trust. This test proves the blending is
//! gone: a mod-added/overriding/tombstoning snapshot no longer changes what a
//! listing shows in the absence of a director — it is exactly the real
//! directory, because nothing authoritative was consulted to say otherwise.
//! (When a director *is* attached and recognises the directory, the listing
//! comes solely from its `readdir` — see `serve_dir_query` in `hook.rs` —
//! which was already merge-free before this task and is unchanged by it.)
use vfs_shim::{install, Engine};

#[test]
fn read_dir_without_a_director_is_exactly_the_real_directory() {
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
    std::fs::write(root.join("over.esp"), vec![0u8; 3]).unwrap(); // would-be override
    std::fs::write(root.join("gone.esp"), b"x").unwrap(); // would-be tombstone
    std::fs::create_dir_all(root.join("realdir")).unwrap();

    // Backing files for the mod override / add — never read, since nothing
    // consults the snapshot for enumeration without a director.
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

    // Real entries are unaffected either way.
    assert!(names.contains(&"real_a.txt".to_string()), "{names:?}");
    assert!(names.contains(&"real_b.txt".to_string()), "{names:?}");
    assert!(names.contains(&"realdir".to_string()), "{names:?}");

    // The snapshot no longer contributes anything without a director:
    // mod-added and mod-only-virtual entries do not appear...
    assert!(
        !names.contains(&"added.esp".to_string()),
        "a mod-added file leaked in without a director consulting the snapshot: {names:?}"
    );
    assert!(
        !names.contains(&"vdir".to_string()),
        "a virtual-only directory leaked in without a director: {names:?}"
    );
    // ...an "override" is really just the real file now (no snapshot to win)...
    let over = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap())
        .find(|e| e.file_name().to_string_lossy() == "over.esp")
        .unwrap();
    assert_eq!(
        over.metadata().unwrap().len(),
        3,
        "no director means no snapshot override — the real file's own size must show"
    );
    // ...and a "tombstone" no longer hides the real file it used to hide.
    assert!(
        names.contains(&"gone.esp".to_string()),
        "no director means no snapshot tombstone — the real file must not be hidden: {names:?}"
    );
}
