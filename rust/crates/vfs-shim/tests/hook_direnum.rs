//! Single-test binary: without a director, nothing under the managed root is
//! reachable at all — not even a directory listing of real, on-disk files.
//!
//! Gate 3, Task 5 flip (was `read_dir_without_a_director_is_exactly_the_real_directory`,
//! asserting that a no-director session still showed exactly the real
//! directory's own contents). Before Task 4, `RootMap::merge_directory`
//! blended the snapshot's virtual children into whatever the OS returned for
//! a directory the shim could not (or, in this harness, did not) ask a real
//! director about; Task 4 removed that blending, so a no-director listing
//! became exactly the real directory — the assertion this test used to make.
//!
//! Task 5 goes one step further: the managed root's own directory node is
//! always `SnapResolution::Dir` in any snapshot (`SnapshotReader::resolve`'s
//! empty-component case resolves to the tree's own root node, which is
//! always a `Dir`), and `RootMap::decide` now denies a `Dir` resolution it
//! has no live director to serve (see that function's own doc comment) —
//! rather than falling through to the real filesystem as it used to. So the
//! root directory itself can no longer even be *opened* without a director,
//! let alone enumerated: `std::fs::read_dir(&root)` now fails outright. This
//! is a stronger, and arguably more honest, statement of the same underlying
//! fact this file always existed to prove — a directory listing is only ever
//! authoritative when a real director backs it (`serve_dir_query` in
//! `hook.rs`, unchanged by this task) — no director now means no access at
//! all, not merely "no snapshot contribution".
use vfs_shim::{install, Engine};

#[test]
fn read_dir_without_a_director_is_denied_outright() {
    let pid = std::process::id();
    let root = std::env::temp_dir().join(format!("vfs-shim-direnum-{pid}"));
    // Backing files live OUTSIDE the root so they do not appear in any listing.
    let backing_dir =
        std::env::temp_dir().join(format!("vfs-shim-direnum-backing-{pid}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&backing_dir).unwrap();

    // Real on-disk contents of the enumerated directory -- before this task,
    // these were visible via passthrough even with no director attached.
    std::fs::write(root.join("real_a.txt"), b"a").unwrap();
    std::fs::write(root.join("real_b.txt"), b"b").unwrap();
    std::fs::create_dir_all(root.join("realdir")).unwrap();

    // Backing file for a mod add -- never read either way: not by a director
    // (there is none), and not by falling through to the snapshot locally
    // (Task 4 removed that; Task 5 removes the passthrough that would have
    // reached the real root directory at all).
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
            entries: vec![e(
                "added.esp",
                EntryKind::File,
                add_backing.to_str().unwrap(),
                10,
            )],
        }])
        .unwrap();
        vfs_shared::bridge::flatten(&tree)
    };
    let engine = Engine::new(root.to_str().unwrap(), snapshot).unwrap();
    let _guard = install(engine).expect("install");

    // No director is attached in this harness at all, so `try_fuse_create`
    // always defers to `decision_for` -> `RootMap::decide`, and the root's
    // own node is always `SnapResolution::Dir` -- denied outright now,
    // rather than passed through to the real, on-disk directory as before.
    let err = std::fs::read_dir(&root).expect_err(
        "without a director, opening the managed root itself must be denied \
         outright -- it must not fall through to the real, on-disk directory",
    );
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::NotFound,
        "expected STATUS_OBJECT_NAME_NOT_FOUND -> ErrorKind::NotFound, got {err:?}"
    );
}
